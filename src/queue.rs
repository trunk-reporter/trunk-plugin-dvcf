//! The work queue: calls waiting to be written, worked off the event thread.
//!
//! Live calls and the calls saved at the last shutdown go first (oldest
//! first), then retries that are due, then catch-up work, on one worker, so
//! the recorder's calls never wait behind a catch-up. Every live job ends in
//! exactly one `call.result`; saved and catch-up jobs send none, unless a live
//! call for the same path joins them, which makes them live. At shutdown the
//! live and saved calls not yet written are handed back, to be saved for the
//! next start; catch-up work is dropped, as the next catch-up finds it again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use trunk_recorder_plugin::{ConcludedCall, Host, Outcome};

/// Where a job came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A `call.concluded`: answered with a `call.result`.
    Live,
    /// A call saved at the last shutdown, before it was written.
    Saved,
    /// A past call the catch-up found with frames and no .dvcf.
    CatchUp,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub call: ConcludedCall,
    pub kind: Kind,
    /// When it was pushed (a retry keeps it).
    pub enqueued: Instant,
    /// Retries so far.
    pub tries: u32,
}

/// What became of one try at a job.
#[derive(Debug, Clone, PartialEq)]
pub enum Done {
    /// Written: how long it took, and the file's size.
    Ok {
        took: Duration,
        bytes: u64,
    },
    Skipped(String),
    Failed(String),
    /// It could not be done just now (the disk, say): try again later.
    Retry(String),
}

#[derive(Debug, Clone)]
pub struct QueueOptions {
    /// The wait before each retry (1 s, 5 s, 15 s); after the last, the job gives up.
    pub retry_after: Vec<Duration>,
}

/// The queue's state and its counters.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Waiting: `front + catch_up + retrying`.
    pub queued: usize,
    /// Live and saved jobs ready to go.
    pub front: usize,
    /// Catch-up jobs ready to go.
    pub catch_up: usize,
    /// Jobs waiting for a retry.
    pub retrying: usize,
    pub in_flight: usize,
    /// Retries scheduled.
    pub retries: u64,
    pub ok: u64,
    /// Jobs that ended failed, for whatever reason.
    pub failed: u64,
    /// `Ok` ends of catch-up jobs (counted in `ok` too).
    pub catch_up_ok: u64,
    /// Bytes written by `Ok` jobs.
    pub bytes: u64,
    /// Time taken by `Ok` jobs, an EWMA (alpha 0.2).
    pub latency_ms: Option<f64>,
    /// Unix seconds.
    pub last_ok_s: Option<u64>,
    /// Unix seconds of the last `Retry` or `Failed`.
    pub last_error_s: Option<u64>,
    pub last_error: String,
}

const LATENCY_ALPHA: f64 = 0.2;

type Work = dyn Fn(&Job) -> Done + Send + Sync;

/// How a job ended.
enum End {
    Ok(Duration),
    Skipped(String),
    Failed(String),
}

/// The job the worker is on.
struct Flight {
    /// Kept so that, unfinished at shutdown, it can be handed back.
    call: ConcludedCall,
    /// Its kind, made `Live` by a live call that joins it while it runs.
    kind: Kind,
    /// Shutdown has handed it back: its end sends nothing.
    answered: bool,
}

#[derive(Default)]
struct Inner {
    /// Live and saved jobs.
    front: VecDeque<Job>,
    catch_up: VecDeque<Job>,
    /// Jobs waiting for a retry, with when it is due.
    retrying: Vec<(Instant, Job)>,
    /// The paths queued or in flight.
    paths: HashSet<String>,
    /// The job in flight, by path.
    flying: HashMap<String, Flight>,
    /// Counted from when the worker takes a job until its end is sent.
    in_flight: usize,
    closing: bool,
    /// Live and saved jobs that asked for a retry once closing: shutdown hands them back.
    unfinished: Vec<ConcludedCall>,
    /// The counters (its queue fields are filled in by [`Queue::snapshot`]).
    stats: Snapshot,
}

impl Inner {
    fn waiting(&self) -> usize {
        self.front.len() + self.catch_up.len() + self.retrying.len()
    }

    /// The next job for the worker: the front lane, then a due retry, then catch-up work.
    fn take(&mut self, now: Instant) -> Option<Job> {
        if let Some(job) = self.front.pop_front() {
            return Some(job);
        }
        let due = self.retrying.iter().enumerate().filter(|(_, (at, _))| *at <= now).min_by_key(|(_, (at, _))| *at);
        if let Some((i, _)) = due {
            return Some(self.retrying.swap_remove(i).1);
        }
        self.catch_up.pop_front()
    }

    /// When the next retry not yet due falls due.
    fn next_due(&self, now: Instant) -> Option<Instant> {
        self.retrying.iter().map(|(at, _)| *at).filter(|at| *at > now).min()
    }

    /// A live call for a path already queued or in flight. A saved or
    /// catch-up job for it becomes live (a waiting catch-up job moves to the
    /// front lane; one in flight is answered as live when it ends): true. A
    /// live job for it answers already: false.
    fn join_live(&mut self, live: Job) -> bool {
        let path = live.call.path.as_str();
        if let Some(job) = self.front.iter_mut().find(|j| j.call.path == path) {
            return make_live(job, live.call);
        }
        if let Some(i) = self.catch_up.iter().position(|j| j.call.path == path) {
            let mut job = self.catch_up.remove(i).expect("found");
            make_live(&mut job, live.call);
            self.front.push_back(job);
            return true;
        }
        if let Some((_, job)) = self.retrying.iter_mut().find(|(_, j)| j.call.path == path) {
            return make_live(job, live.call);
        }
        match self.flying.get_mut(path) {
            Some(flight) if flight.kind != Kind::Live && !flight.answered => {
                flight.kind = Kind::Live;
                true
            }
            _ => false,
        }
    }

    fn error(&mut self, message: &str) {
        self.stats.last_error = message.to_string();
        self.stats.last_error_s = Some(unix_now());
    }

    fn ok(&mut self, took: Duration, bytes: u64, kind: Kind) {
        let s = &mut self.stats;
        let ms = took.as_secs_f64() * 1000.0;
        s.latency_ms = Some(s.latency_ms.map_or(ms, |l| l + LATENCY_ALPHA * (ms - l)));
        s.last_ok_s = Some(unix_now());
        s.ok += 1;
        s.bytes += bytes;
        if kind == Kind::CatchUp {
            s.catch_up_ok += 1;
        }
    }
}

/// Makes a waiting job live, with the live call's record: false if it was already.
fn make_live(job: &mut Job, call: ConcludedCall) -> bool {
    if job.kind == Kind::Live {
        return false;
    }
    job.kind = Kind::Live;
    job.call = call;
    true
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct Shared {
    host: Host,
    opts: QueueOptions,
    work: Box<Work>,
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn worker(&self) {
        let mut inner = self.lock();
        loop {
            if inner.closing {
                return;
            }
            let now = Instant::now();
            if let Some(job) = inner.take(now) {
                inner.flying.insert(job.call.path.clone(), Flight { call: job.call.clone(), kind: job.kind, answered: false });
                inner.in_flight += 1;
                drop(inner);
                self.run(job);
                inner = self.lock();
                continue;
            }
            inner = match inner.next_due(now) {
                Some(at) => self.changed.wait_timeout(inner, at - now).unwrap_or_else(|e| e.into_inner()).0,
                None => self.changed.wait(inner).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    /// Works `job` (taken, and counted in flight) to its end or its next retry.
    fn run(&self, mut job: Job) {
        let done = catch_unwind(AssertUnwindSafe(|| (self.work)(&job))).unwrap_or_else(|_| Done::Failed("the work panicked".into()));
        let mut inner = self.lock();
        // Whether its end goes to the recorder is settled as the flight ends, under the lock.
        let answered = match inner.flying.remove(&job.call.path) {
            Some(flight) => {
                job.kind = flight.kind;
                flight.answered
            }
            None => false,
        };
        let end = match done {
            Done::Ok { took, bytes } => {
                inner.ok(took, bytes, job.kind);
                End::Ok(took)
            }
            Done::Skipped(m) => End::Skipped(m),
            Done::Failed(m) => {
                inner.error(&m);
                inner.stats.failed += 1;
                End::Failed(m)
            }
            Done::Retry(m) => {
                inner.error(&m);
                let tries = job.tries as usize;
                if answered || inner.closing {
                    // Shutdown has it, or will take it from `unfinished`; a catch-up job is let go.
                    if !answered && job.kind != Kind::CatchUp {
                        inner.unfinished.push(job.call.clone());
                    }
                    inner.paths.remove(&job.call.path);
                    return self.finish(inner);
                }
                if tries < self.opts.retry_after.len() {
                    let message = format!("retry {}/{} for {}: {m}", tries + 1, self.opts.retry_after.len(), job.call.path);
                    job.tries += 1;
                    inner.stats.retries += 1;
                    inner.retrying.push((Instant::now() + self.opts.retry_after[tries], job));
                    inner.in_flight -= 1;
                    self.changed.notify_all();
                    drop(inner);
                    self.host.warn(message);
                    return;
                }
                inner.stats.failed += 1;
                End::Failed(format!("gave up: {m}"))
            }
        };
        inner.paths.remove(&job.call.path);
        drop(inner);
        if job.kind == Kind::Live && !answered {
            self.answer(&job.call.path, &end);
        }
        // Counted in flight until its end is sent, so an empty queue has sent everything.
        self.finish(self.lock())
    }

    /// The job in flight is over.
    fn finish(&self, mut inner: MutexGuard<'_, Inner>) {
        inner.in_flight -= 1;
        self.changed.notify_all();
    }

    /// Tells the recorder of a live job's end. Called with no lock held.
    fn answer(&self, path: &str, end: &End) {
        match end {
            End::Ok(took) => self.host.call_result_took(path, Outcome::Ok, "", "", *took),
            End::Skipped(m) => self.host.call_result(path, Outcome::Skipped, m.as_str(), ""),
            End::Failed(m) => self.host.call_result(path, Outcome::Failed, m.as_str(), ""),
        }
    }
}

/// A priority queue of calls with its worker thread.
pub struct Queue {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Queue {
    /// Starts the worker thread (`dvcf-worker`) running `work` on each job.
    pub fn start(host: Host, opts: QueueOptions, work: impl Fn(&Job) -> Done + Send + Sync + 'static) -> Queue {
        let shared = Arc::new(Shared { host, opts, work: Box::new(work), inner: Mutex::new(Inner::default()), changed: Condvar::new() });
        let s = shared.clone();
        let worker = thread::Builder::new().name("dvcf-worker".into()).spawn(move || s.worker()).expect("start the queue worker");
        Queue { shared, worker: Mutex::new(Some(worker)) }
    }

    /// Queues `call`. When a job with its path is queued or in flight, a live
    /// call joins a saved or catch-up job for it, which then answers the
    /// recorder (true); anything else is refused (false, and nothing happens).
    /// Once shutdown has begun, a live call is answered at once with "shutting
    /// down" (true), unless a live job for its path is in flight and will
    /// answer for itself (false); any other is dropped (false).
    pub fn push(&self, call: ConcludedCall, kind: Kind) -> bool {
        let s = &self.shared;
        let job = Job { call, kind, enqueued: Instant::now(), tries: 0 };
        let mut inner = s.lock();
        // Once closing, nothing joins a job: shutdown may already have handed
        // back what it will, so a joined end could go nowhere.
        if inner.closing {
            let answers = inner.flying.get(&job.call.path).is_some_and(|f| f.kind == Kind::Live && !f.answered);
            if job.kind != Kind::Live || answers {
                return false;
            }
            inner.stats.failed += 1;
            drop(inner);
            s.answer(&job.call.path, &End::Failed("shutting down".into()));
            return true;
        }
        if inner.paths.contains(&job.call.path) {
            let joined = job.kind == Kind::Live && inner.join_live(job);
            s.changed.notify_all();
            return joined;
        }
        inner.paths.insert(job.call.path.clone());
        if job.kind == Kind::CatchUp {
            inner.catch_up.push_back(job)
        } else {
            inner.front.push_back(job)
        }
        s.changed.notify_all();
        true
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.shared.lock();
        Snapshot {
            queued: inner.waiting(),
            front: inner.front.len(),
            catch_up: inner.catch_up.len(),
            retrying: inner.retrying.len(),
            in_flight: inner.in_flight,
            ..inner.stats.clone()
        }
    }

    /// Catch-up jobs waiting, ready or for a retry.
    pub fn catch_up_waiting(&self) -> usize {
        let inner = self.shared.lock();
        inner.catch_up.len() + inner.retrying.iter().filter(|(_, j)| j.kind == Kind::CatchUp).count()
    }

    /// Jobs queued and in flight.
    pub fn len(&self) -> usize {
        let inner = self.shared.lock();
        inner.waiting() + inner.in_flight
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stops taking work and hands back the calls to save. Waiting catch-up
    /// jobs are dropped; waiting live and saved jobs (retries too) are taken
    /// off the queue; the job in flight gets up to 0.8 × `grace` to finish. A
    /// live or saved job still in flight then is handed back too, and its end,
    /// should it come, sends nothing. No `call.result` goes for any of them.
    /// Returns the calls in queue order, the one in flight last.
    pub fn shutdown(&self, grace: Duration) -> Vec<ConcludedCall> {
        let s = &self.shared;
        let deadline = Instant::now() + grace.mul_f64(0.8);
        let mut saved: Vec<ConcludedCall> = {
            let mut guard = s.lock();
            let inner = &mut *guard;
            inner.closing = true;
            let mut retrying = std::mem::take(&mut inner.retrying);
            retrying.sort_by_key(|(at, _)| *at);
            let jobs: Vec<Job> = inner.front.drain(..).chain(retrying.into_iter().map(|(_, job)| job)).chain(inner.catch_up.drain(..)).collect();
            for job in &jobs {
                inner.paths.remove(&job.call.path);
            }
            s.changed.notify_all();
            jobs.into_iter().filter(|j| j.kind != Kind::CatchUp).map(|j| j.call).collect()
        };
        let mut inner = s.lock();
        while inner.in_flight > 0 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            inner = s.changed.wait_timeout(inner, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
        saved.append(&mut inner.unfinished);
        for flight in inner.flying.values_mut().filter(|f| f.kind != Kind::CatchUp && !f.answered) {
            flight.answered = true;
            saved.push(flight.call.clone());
        }
        let idle = inner.in_flight == 0;
        drop(inner);
        if idle && let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = worker.join();
        }
        saved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use trunk_recorder_plugin::{Level, PluginMessage, testing};

    fn opts() -> QueueOptions {
        QueueOptions { retry_after: vec![Duration::from_millis(20); 3] }
    }

    fn call(dir: &std::path::Path, tg: u32) -> ConcludedCall {
        testing::call(dir, "sys1", tg)
    }

    /// Polls `cond` until it holds, failing the test after 10 s.
    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A gate `work` can block on until the test opens it (by dropping the sender).
    fn gate() -> (mpsc::Sender<()>, Arc<Mutex<mpsc::Receiver<()>>>) {
        let (tx, rx) = mpsc::channel();
        (tx, Arc::new(Mutex::new(rx)))
    }

    type Started = Arc<Mutex<Vec<(Kind, u32)>>>;

    /// A queue whose work blocks until the gate opens, keeping (kind, tg) of each job it starts.
    fn blocking(host: Host) -> (Queue, Started, mpsc::Sender<()>) {
        let (open, shut) = gate();
        let started = Started::default();
        let s = started.clone();
        let q = Queue::start(host, opts(), move |job: &Job| {
            s.lock().unwrap().push((job.kind, job.call.call.talkgroup));
            let _ = shut.lock().unwrap().recv();
            Done::Ok { took: Duration::ZERO, bytes: 0 }
        });
        (q, started, open)
    }

    fn result(path: &str, outcome: Outcome, message: &str) -> (String, Outcome, String, String) {
        (path.to_string(), outcome, message.to_string(), String::new())
    }

    #[test]
    fn the_front_lane_goes_before_catch_up() {
        let (host, _out) = testing::capture();
        let (q, started, open) = blocking(host);
        let dir = testing::temp_dir("queue-order");
        assert!(q.push(call(&dir, 1), Kind::CatchUp));
        wait_for("the worker to be busy", || !started.lock().unwrap().is_empty());
        assert!(q.push(call(&dir, 2), Kind::CatchUp));
        assert!(q.push(call(&dir, 3), Kind::Live));
        assert!(q.push(call(&dir, 4), Kind::Saved));
        let s = q.snapshot();
        assert_eq!((s.queued, s.front, s.catch_up, s.retrying, s.in_flight), (3, 2, 1, 0, 1));
        assert_eq!(q.catch_up_waiting(), 1);
        assert_eq!(q.len(), 4);
        drop(open);
        wait_for("all four done", || started.lock().unwrap().len() == 4 && q.is_empty());
        assert_eq!(*started.lock().unwrap(), vec![(Kind::CatchUp, 1), (Kind::Live, 3), (Kind::Saved, 4), (Kind::CatchUp, 2)]);
    }

    #[test]
    fn results_go_back_for_live_only() {
        let (host, out) = testing::capture();
        let worked = Arc::new(Mutex::new(Vec::<u32>::new()));
        let w = worked.clone();
        let q = Queue::start(host, opts(), move |job: &Job| {
            let tg = job.call.call.talkgroup;
            w.lock().unwrap().push(tg);
            match tg {
                2 => Done::Skipped("analog".into()),
                3 => Done::Failed("no frames".into()),
                _ => Done::Ok { took: Duration::from_millis(7), bytes: 64 },
            }
        });
        let dir = testing::temp_dir("queue-results");
        let calls: Vec<_> = (1..=5).map(|tg| call(&dir, tg)).collect();
        let p: Vec<String> = calls.iter().map(|c| c.path.clone()).collect();
        let kinds = [Kind::Live, Kind::Live, Kind::Live, Kind::CatchUp, Kind::Saved];
        for (c, kind) in calls.into_iter().zip(kinds) {
            assert!(q.push(c, kind));
        }
        wait_for("all five done", || worked.lock().unwrap().len() == 5 && q.is_empty());

        let o = out.output();
        assert_eq!(o.results(), vec![result(&p[0], Outcome::Ok, ""), result(&p[1], Outcome::Skipped, "analog"), result(&p[2], Outcome::Failed, "no frames")]);
        let took = o.messages.iter().find_map(|m| match m {
            PluginMessage::CallResult { path, took_ms, .. } if *path == p[0] => Some(*took_ms),
            _ => None,
        });
        assert_eq!(took, Some(Some(7)));
        let s = q.snapshot();
        assert_eq!((s.ok, s.failed, s.catch_up_ok, s.bytes), (3, 1, 1, 192));
        assert!(s.latency_ms.is_some() && s.last_ok_s.is_some());
        assert_eq!(s.last_error, "no frames");
        assert!(s.last_error_s.is_some());
    }

    #[test]
    fn retries_then_gives_up() {
        let (host, out) = testing::capture();
        let tries = Arc::new(Mutex::new(0u32));
        let t = tries.clone();
        let q = Queue::start(host, opts(), move |_: &Job| {
            *t.lock().unwrap() += 1;
            Done::Retry("disk".into())
        });
        let dir = testing::temp_dir("queue-retry");
        let c = call(&dir, 1);
        let p = c.path.clone();
        assert!(q.push(c, Kind::Live));
        wait_for("a result", || !out.output().results().is_empty() && q.is_empty());

        assert_eq!(*tries.lock().unwrap(), 4);
        let o = out.output();
        assert_eq!(o.results(), vec![result(&p, Outcome::Failed, "gave up: disk")]);
        let warns: Vec<String> = o.logs().into_iter().filter(|(l, _)| *l == Level::Warn).map(|(_, m)| m).collect();
        assert_eq!(warns, (1..=3).map(|n| format!("retry {n}/3 for {p}: disk")).collect::<Vec<_>>());
        let s = q.snapshot();
        assert_eq!((s.retries, s.failed, s.ok), (3, 1, 0));
        assert_eq!(s.last_error, "disk");
    }

    #[test]
    fn a_duplicate_path_is_queued_once_and_a_live_push_makes_it_live() {
        let (host, out) = testing::capture();
        let (q, started, open) = blocking(host);
        let dir = testing::temp_dir("queue-dup");
        assert!(q.push(call(&dir, 10), Kind::Saved));
        wait_for("the worker to be busy", || !started.lock().unwrap().is_empty());
        let c = call(&dir, 1);
        let pc = c.path.clone();
        assert!(q.push(c.clone(), Kind::CatchUp));
        assert!(!q.push(c.clone(), Kind::CatchUp), "queued: refused");
        assert!(!q.push(c.clone(), Kind::Saved), "whatever its kind");
        assert!(q.push(c.clone(), Kind::Live), "a live push makes it live");
        let s = q.snapshot();
        assert_eq!((s.front, s.catch_up), (1, 0), "moved to the front lane");
        assert!(!q.push(c, Kind::Live), "now live: a second live push is refused");
        let d = call(&dir, 2);
        let pd = d.path.clone();
        assert!(q.push(d.clone(), Kind::Live));
        assert!(!q.push(d, Kind::Live), "the same live call twice: refused");
        drop(open);
        wait_for("all three done", || started.lock().unwrap().len() == 3 && q.is_empty());

        assert_eq!(*started.lock().unwrap(), vec![(Kind::Saved, 10), (Kind::Live, 1), (Kind::Live, 2)], "each worked once");
        assert_eq!(out.output().results(), vec![result(&pc, Outcome::Ok, ""), result(&pd, Outcome::Ok, "")]);
    }

    #[test]
    fn a_live_push_joins_a_catch_up_job_in_flight() {
        let (host, out) = testing::capture();
        let (q, started, open) = blocking(host);
        let dir = testing::temp_dir("queue-join-flight");
        let c = call(&dir, 1);
        let pc = c.path.clone();
        assert!(q.push(c.clone(), Kind::CatchUp));
        wait_for("the catch-up job to start", || !started.lock().unwrap().is_empty());
        assert!(q.push(c.clone(), Kind::Live), "joins the job in flight");
        assert!(!q.push(c, Kind::Live), "a second live push is refused");
        drop(open);
        wait_for("its end", || q.is_empty());

        assert_eq!(out.output().results(), vec![result(&pc, Outcome::Ok, "")], "exactly one call.result");
        assert_eq!(started.lock().unwrap().len(), 1, "worked once");
        assert_eq!(q.snapshot().catch_up_ok, 0, "it ended live");
    }

    #[test]
    fn shutdown_returns_what_to_save_and_sends_nothing() {
        let (host, out) = testing::capture();
        let (q, started, open) = blocking(host);
        let dir = testing::temp_dir("queue-shutdown");
        let calls: Vec<_> = (1..=6).map(|tg| call(&dir, tg)).collect();
        let p: Vec<String> = calls.iter().map(|c| c.path.clone()).collect();
        assert!(q.push(calls[0].clone(), Kind::Live));
        wait_for("the worker to be busy", || !started.lock().unwrap().is_empty());
        assert!(q.push(calls[1].clone(), Kind::Live));
        assert!(q.push(calls[2].clone(), Kind::Saved));
        assert!(q.push(calls[3].clone(), Kind::CatchUp));

        let t = Instant::now();
        let saved = q.shutdown(Duration::from_millis(100));
        let took = t.elapsed();
        assert!(took < Duration::from_millis(150), "shutdown took {took:?}");
        assert!(took >= Duration::from_millis(80), "it waits 0.8 × grace for the job in flight: {took:?}");
        let saved: Vec<String> = saved.into_iter().map(|c| c.path).collect();
        assert_eq!(saved, vec![p[1].clone(), p[2].clone(), p[0].clone()], "the waiting in queue order, then the one in flight; no catch-up");
        assert!(out.output().results().is_empty());
        assert_eq!(q.catch_up_waiting(), 0);

        drop(open);
        wait_for("job 1 to finish", || q.is_empty());
        assert!(out.output().results().is_empty(), "its late end sends nothing");
        assert_eq!(started.lock().unwrap().len(), 1, "nothing else ran");

        assert!(q.push(calls[4].clone(), Kind::Live), "after shutdown, a live call is answered at once");
        assert!(!q.push(calls[5].clone(), Kind::CatchUp), "others are dropped");
        assert_eq!(out.output().results(), vec![result(&p[4], Outcome::Failed, "shutting down")]);
        assert!(q.is_empty());
    }

    #[test]
    fn a_live_push_after_shutdown_never_joins_a_catch_up_job_in_flight() {
        let (host, out) = testing::capture();
        let (open, shut) = gate();
        let started = Arc::new(Mutex::new(0u32));
        let s = started.clone();
        let q = Queue::start(host, opts(), move |_: &Job| {
            *s.lock().unwrap() += 1;
            let _ = shut.lock().unwrap().recv();
            Done::Retry("disk".into())
        });
        let dir = testing::temp_dir("queue-shutdown-join");
        let c = call(&dir, 1);
        let p = c.path.clone();
        assert!(q.push(c.clone(), Kind::CatchUp));
        wait_for("the catch-up job to start", || *started.lock().unwrap() == 1);
        assert!(q.shutdown(Duration::from_millis(20)).is_empty(), "a catch-up job in flight is not handed back");

        assert!(q.push(c, Kind::Live), "answered at once, not joined to the catch-up job");
        let want = vec![result(&p, Outcome::Failed, "shutting down")];
        assert_eq!(out.output().results(), want);
        drop(open);
        wait_for("the catch-up job to end", || q.is_empty());
        assert_eq!(out.output().results(), want, "its late end sends nothing more");
    }

    #[test]
    fn a_panicking_job_is_failed_not_fatal() {
        let (host, out) = testing::capture();
        let q = Queue::start(host, opts(), |job: &Job| {
            if job.call.call.talkgroup == 1 {
                panic!("boom");
            }
            Done::Ok { took: Duration::from_millis(1), bytes: 8 }
        });
        let dir = testing::temp_dir("queue-panic");
        let (c1, c2) = (call(&dir, 1), call(&dir, 2));
        let (p1, p2) = (c1.path.clone(), c2.path.clone());
        assert!(q.push(c1, Kind::Live));
        assert!(q.push(c2, Kind::Live));
        wait_for("both results", || out.output().results().len() == 2 && q.is_empty());

        assert_eq!(out.output().results(), vec![result(&p1, Outcome::Failed, "the work panicked"), result(&p2, Outcome::Ok, "")]);
        let s = q.snapshot();
        assert_eq!((s.ok, s.failed), (1, 1));
    }
}
