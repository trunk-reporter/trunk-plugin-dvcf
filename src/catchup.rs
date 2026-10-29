//! The catch-up pass: pages the recorder's call store through `calls.query`,
//! newest first, and queues the past calls that still have a frames file and
//! no `.dvcf`. It queues at most a budget of catch-up work; past that it
//! pauses, and the next pass takes up the rest.

use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use trunk_recorder_plugin::{CallFiles, CallsPage, CallsQuery, ConcludedCall, Host, StoredCall};

use crate::output::target;
use crate::queue::{Kind, Queue};
use crate::work::{Ctx, skip_reason};

const DAY_MS: i64 = 86_400_000;
/// Calls asked for on a page.
pub const PAGE: usize = 500;
/// The catch-up jobs the queue may hold before a pass pauses.
pub const BUDGET: usize = 2000;
/// A page not come this long after it was asked ends the pass.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a pass is for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// At startup: every call of the last `catchUpDays` days.
    Start,
    /// On a timer: stops at the first page with nothing queued.
    Periodic,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Start => "start",
            Mode::Periodic => "periodic",
        }
    }
}

/// The running pass.
struct Pass {
    request: u64,
    /// The window's lower bound, asked on every page.
    from: Option<i64>,
    /// Ends at the first page with nothing queued (a periodic pass, unless the last pass paused).
    stop_early: bool,
    /// When the page awaited was asked for.
    asked: Instant,
    queued: u64,
    skipped: u64,
    settled: u64,
    pages: u64,
}

/// What became of one page's calls.
#[derive(Default)]
struct Tally {
    queued: u64,
    skipped: u64,
    settled: u64,
    /// The budget was reached before the page's end.
    paused: bool,
}

pub struct CatchUp {
    host: Host,
    ctx: Ctx,
    queue: Arc<Queue>,
    budget: usize,
    pass: Option<Pass>,
    /// The last pass paused with calls left: the next periodic one goes through the whole window.
    resume: bool,
    /// The call store's answer when the last pass ended on an error.
    store_error: Option<String>,
    /// Jobs queued by every pass since the start.
    queued_total: u64,
}

/// A read lock, even on a lock a panic poisoned: the data behind it is plain settings.
fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

impl CatchUp {
    /// A catch-up that queues on `queue` and pauses once it holds `budget` catch-up jobs.
    pub fn new(host: Host, ctx: Ctx, queue: Arc<Queue>, budget: usize) -> CatchUp {
        CatchUp { host, ctx, queue, budget, pass: None, resume: false, store_error: None, queued_total: 0 }
    }

    pub fn running(&self) -> bool {
        self.pass.is_some()
    }

    /// The call store's answer when the last pass ended on one; cleared by a pass that ends without.
    pub fn store_error(&self) -> Option<String> {
        self.store_error.clone()
    }

    /// Jobs queued by every pass since the start.
    pub fn queued_total(&self) -> u64 {
        self.queued_total
    }

    /// Starts a pass over the last `catchUpDays` days (all of them when it is 0); false when one is running.
    pub fn begin(&mut self, mode: Mode, now_ms: i64) -> bool {
        if self.pass.is_some() {
            return false;
        }
        let days = read_lock(&self.ctx.config).catch_up_days;
        let from = (days > 0).then(|| now_ms - i64::from(days) * DAY_MS);
        let stop_early = match mode {
            Mode::Periodic => !std::mem::take(&mut self.resume),
            Mode::Start => {
                self.resume = false;
                false
            }
        };
        self.host.info(format!("catch-up started ({})", mode.label()));
        let request = self.host.calls(CallsQuery { from, limit: Some(PAGE), ..Default::default() });
        self.pass = Some(Pass { request, from, stop_early, asked: Instant::now(), queued: 0, skipped: 0, settled: 0, pages: 0 });
        true
    }

    /// Ends the pass when the page it asked for has not come in `PAGE_TIMEOUT`; true when it did.
    pub fn check_timeout(&mut self, now: Instant) -> bool {
        let Some(pass) = self.pass.take_if(|p| now.saturating_duration_since(p.asked) >= PAGE_TIMEOUT) else { return false };
        self.host.warn(format!("catch-up: no answer from the recorder's call store in {} s", PAGE_TIMEOUT.as_secs()));
        self.finish(&pass, None);
        true
    }

    /// Handles a page of the recorder's call store; false when it is not this pass's.
    pub fn on_page(&mut self, request: u64, page: CallsPage) -> bool {
        let Some(mut pass) = self.pass.take() else { return false };
        if pass.request != request {
            self.pass = Some(pass);
            return false;
        }
        pass.pages += 1;
        if let Some(e) = page.error {
            self.host.warn(format!("catch-up: the recorder's call store answered: {e}"));
            self.finish(&pass, Some(e));
            return true;
        }
        let tally = self.work_page(&page.entries);
        pass.queued += tally.queued;
        pass.skipped += tally.skipped;
        pass.settled += tally.settled;
        let stop = pass.stop_early && tally.queued == 0;
        if tally.paused {
            self.host.info(format!("catch-up paused: {} calls queued, the rest waits for the next pass", self.queue.catch_up_waiting()));
            self.resume = true;
            self.finish(&pass, None);
        } else if let Some(cursor) = page.next.filter(|_| !stop) {
            pass.request = self.host.calls(CallsQuery { from: pass.from, cursor: Some(cursor), limit: Some(PAGE), ..Default::default() });
            pass.asked = Instant::now();
            self.pass = Some(pass);
        } else {
            self.finish(&pass, None);
        }
        true
    }

    /// Ends `pass`: its totals logged, the store's `error` (if any) remembered in place of the last.
    fn finish(&mut self, pass: &Pass, error: Option<String>) {
        self.queued_total += pass.queued;
        self.store_error = error;
        self.host.info(format!("catch-up: {} queued, {} skipped, {} already written, {} pages", pass.queued, pass.skipped, pass.settled, pass.pages));
    }

    /// Queues the calls with a frames file and no `.dvcf`, until the queue holds the budget of catch-up work.
    /// A call the skip rules leave out is counted skipped; one already written, settled.
    fn work_page(&self, entries: &[StoredCall]) -> Tally {
        let output_dir = read_lock(&self.ctx.config).output_dir();
        let mut tally = Tally::default();
        for e in entries {
            let call = self.concluded(e);
            if skip_reason(&self.ctx, &call).is_some() {
                tally.skipped += 1;
                continue;
            }
            if target(output_dir.as_deref(), &self.ctx.capture_dir, &call.path).exists() {
                tally.settled += 1;
                continue;
            }
            if self.queue.catch_up_waiting() >= self.budget {
                tally.paused = true;
                break;
            }
            if self.queue.push(call, Kind::CatchUp) {
                tally.queued += 1;
            }
        }
        tally
    }

    /// The call as the recorder would have concluded it: its files under the capture folder, its system by short name (else the first).
    fn concluded(&self, e: &StoredCall) -> ConcludedCall {
        let system = read_lock(&self.ctx.systems).iter().position(|s| s.short_name == e.record.short_name).unwrap_or(0) as u16;
        let dir = &self.ctx.capture_dir;
        ConcludedCall {
            path: e.path.clone(),
            system,
            call: e.record.clone(),
            files: CallFiles { json: dir.join(format!("{}.json", e.path)), wav: dir.join(format!("{}.wav", e.path)), m4a: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::mpsc;

    use trunk_core::trunk::sdr::SdrMode;
    use trunk_recorder_plugin::testing::{self, Captured};
    use trunk_recorder_plugin::{Level, PluginMessage};

    use crate::config::Config;
    use crate::fixtures;
    use crate::queue::{Done, QueueOptions};
    use crate::work::SystemView;

    const NOW: i64 = 100 * DAY_MS;

    type Seen = Arc<Mutex<Vec<(Kind, String)>>>;

    struct Fx {
        catchup: CatchUp,
        out: Captured,
        seen: Seen,
        queue: Arc<Queue>,
        ctx: Ctx,
        /// Dropping it lets a blocked worker go on.
        _gate: Option<mpsc::Sender<()>>,
    }

    fn ctx_for(dir: &Path) -> Ctx {
        Ctx {
            config: Arc::new(RwLock::new(Config::default())),
            systems: Arc::new(RwLock::new(vec![SystemView { short_name: "sys1".into(), enabled: true }])),
            capture_dir: dir.to_path_buf(),
            host_version: "0.3.0".into(),
            skipped: Arc::default(),
        }
    }

    /// A catch-up over `dir` whose queue's worker records what it saw and answers `Ok`.
    fn fixture(dir: &Path) -> Fx {
        let (host, out) = testing::capture();
        let seen = Seen::default();
        let s = seen.clone();
        let queue = Arc::new(Queue::start(host.clone(), QueueOptions { retry_after: vec![Duration::from_millis(20)] }, move |job| {
            s.lock().unwrap().push((job.kind, job.call.path.clone()));
            Done::Ok { took: Duration::ZERO, bytes: 0 }
        }));
        let ctx = ctx_for(dir);
        let catchup = CatchUp::new(host, ctx.clone(), queue.clone(), BUDGET);
        Fx { catchup, out, seen, queue, ctx, _gate: None }
    }

    /// A catch-up with `budget` whose queue's worker is kept busy on a saved call until the fixture is dropped,
    /// so every catch-up job it queues stays waiting.
    fn blocked(dir: &Path, budget: usize) -> Fx {
        let (host, out) = testing::capture();
        let seen = Seen::default();
        let s = seen.clone();
        let (open, shut) = mpsc::channel::<()>();
        let shut = Mutex::new(shut);
        let queue = Arc::new(Queue::start(host.clone(), QueueOptions { retry_after: vec![Duration::from_millis(20)] }, move |job| {
            s.lock().unwrap().push((job.kind, job.call.path.clone()));
            let _ = shut.lock().unwrap().recv();
            Done::Ok { took: Duration::ZERO, bytes: 0 }
        }));
        let (busy, _) = fixtures::sdr_call(dir, SdrMode::P25Fdma, 99, 2);
        assert!(queue.push(busy, Kind::Saved));
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "timed out waiting for the worker to be busy");
            std::thread::sleep(Duration::from_millis(5));
        }
        let ctx = ctx_for(dir);
        let catchup = CatchUp::new(host, ctx.clone(), queue.clone(), budget);
        Fx { catchup, out, seen, queue, ctx, _gate: Some(open) }
    }

    /// A call with a frames file under `dir`, as the call store lists it, and as the recorder concluded it.
    fn convertible(dir: &Path, tg: u32) -> (StoredCall, ConcludedCall) {
        let (call, _) = fixtures::sdr_call(dir, SdrMode::P25Fdma, tg, 4);
        (StoredCall { path: call.path.clone(), record: call.call.clone(), audio: true, json: true }, call)
    }

    /// A call with a frames file and a `.dvcf` already beside it.
    fn written(dir: &Path, tg: u32) -> StoredCall {
        let (stored, call) = convertible(dir, tg);
        std::fs::write(call.files.json.with_extension("dvcf"), b"dvcf").unwrap();
        stored
    }

    fn page(entries: Vec<StoredCall>, next: Option<&str>) -> CallsPage {
        CallsPage { entries, next: next.map(str::to_owned), error: None }
    }

    fn queries(f: &Fx) -> Vec<(u64, CallsQuery)> {
        f.out
            .output()
            .messages
            .into_iter()
            .filter_map(|m| match m {
                PluginMessage::CallsQuery { request, query } => Some((request, query)),
                _ => None,
            })
            .collect()
    }

    fn logs(f: &Fx, level: Level) -> Vec<String> {
        f.out.output().logs().into_iter().filter(|(l, _)| *l == level).map(|(_, m)| m).collect()
    }

    /// Waits until the queue is drained and returns what the worker saw, sorted by path.
    fn drained(f: &Fx, expect: usize) -> Vec<(Kind, String)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while f.seen.lock().unwrap().len() < expect || !f.queue.is_empty() {
            assert!(Instant::now() < deadline, "timed out waiting for the worker");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut seen = f.seen.lock().unwrap().clone();
        seen.sort_by(|a, b| a.1.cmp(&b.1));
        seen
    }

    #[test]
    fn a_start_pass_queues_what_has_frames_and_no_dvcf_and_pages_to_the_end() {
        let dir = testing::temp_dir("catchup");
        let mut f = fixture(&dir);
        let (a, _) = convertible(&dir, 1);
        let b = written(&dir, 2);
        let (c, c_call) = convertible(&dir, 3);
        std::fs::remove_file(c_call.files.json.with_extension("sdr")).unwrap();
        let (mut d, _) = convertible(&dir, 4);
        d.record.audio_type = "analog".into();

        assert!(f.catchup.begin(Mode::Start, NOW));
        assert!(f.catchup.running());
        let q = queries(&f);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].1, CallsQuery { from: Some(NOW - 30 * DAY_MS), limit: Some(500), ..Default::default() });

        assert!(f.catchup.on_page(q[0].0, page(vec![a.clone(), b], Some("500"))));
        let q = queries(&f);
        assert_eq!(q.len(), 2);
        assert_eq!(q[1].1, CallsQuery { from: Some(NOW - 30 * DAY_MS), cursor: Some("500".into()), limit: Some(500), ..Default::default() });
        assert!(f.catchup.running());

        assert!(f.catchup.on_page(q[1].0, page(vec![c, d], None)));
        assert!(!f.catchup.running());
        assert_eq!(queries(&f).len(), 2);
        assert_eq!(drained(&f, 1), [(Kind::CatchUp, a.path.clone())]);
        assert_eq!(f.catchup.queued_total(), 1);
        assert_eq!(f.catchup.store_error(), None);
        assert!(f.ctx.skipped.lock().unwrap().is_empty(), "a pass's skips are not counted as the work's");
        let infos = logs(&f, Level::Info);
        assert!(infos.contains(&"catch-up started (start)".to_string()), "{infos:?}");
        assert!(infos.contains(&"catch-up: 1 queued, 2 skipped, 1 already written, 2 pages".to_string()), "{infos:?}");
        assert!(logs(&f, Level::Warn).is_empty());
    }

    #[test]
    fn the_calls_files_are_found_under_the_capture_folder() {
        let dir = testing::temp_dir("catchup");
        let (host, _out) = testing::capture();
        let files = Arc::new(Mutex::new(Vec::new()));
        let w = files.clone();
        let queue = Arc::new(Queue::start(host.clone(), QueueOptions { retry_after: vec![] }, move |job| {
            w.lock().unwrap().push((job.call.system, job.call.files.clone()));
            Done::Ok { took: Duration::ZERO, bytes: 0 }
        }));
        let ctx = ctx_for(&dir);
        ctx.systems.write().unwrap().insert(0, SystemView { short_name: "other".into(), enabled: true });
        let mut catchup = CatchUp::new(host, ctx, queue, BUDGET);
        let (a, call) = convertible(&dir, 5);
        assert!(catchup.begin(Mode::Start, NOW));
        assert!(catchup.on_page(1, page(vec![a], None)));
        let deadline = Instant::now() + Duration::from_secs(10);
        while files.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "timed out waiting for the worker");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (system, got) = files.lock().unwrap()[0].clone();
        assert_eq!(system, 1, "sys1 is the second system");
        assert_eq!((got.json, got.wav, got.m4a), (call.files.json, call.files.wav, None));
    }

    #[test]
    fn a_periodic_pass_stops_at_the_first_page_with_nothing_queued() {
        let dir = testing::temp_dir("catchup");
        let mut f = fixture(&dir);
        let b = written(&dir, 2);
        assert!(f.catchup.begin(Mode::Periodic, NOW));
        let req = queries(&f)[0].0;
        assert!(f.catchup.on_page(req, page(vec![b], Some("500"))));
        assert_eq!(queries(&f).len(), 1, "no second page asked");
        assert!(!f.catchup.running());
        let infos = logs(&f, Level::Info);
        assert!(infos.contains(&"catch-up started (periodic)".to_string()), "{infos:?}");
        assert!(infos.contains(&"catch-up: 0 queued, 0 skipped, 1 already written, 1 pages".to_string()), "{infos:?}");
    }

    #[test]
    fn zero_days_means_no_lower_bound() {
        let dir = testing::temp_dir("catchup");
        let mut f = fixture(&dir);
        f.ctx.config.write().unwrap().catch_up_days = 0;
        assert!(f.catchup.begin(Mode::Start, NOW));
        assert_eq!(queries(&f)[0].1, CallsQuery { from: None, limit: Some(500), ..Default::default() });
        // A live setting: the next pass reads it again.
        let req = queries(&f)[0].0;
        assert!(f.catchup.on_page(req, page(vec![], None)));
        f.ctx.config.write().unwrap().catch_up_days = 2;
        assert!(f.catchup.begin(Mode::Start, NOW));
        assert_eq!(queries(&f)[1].1.from, Some(NOW - 2 * DAY_MS));
    }

    #[test]
    fn the_store_being_off_ends_the_pass_and_is_remembered() {
        let dir = testing::temp_dir("catchup");
        let mut f = fixture(&dir);
        assert!(f.catchup.begin(Mode::Start, NOW));
        let req = queries(&f)[0].0;
        let p = CallsPage { error: Some("the call store is off".into()), ..Default::default() };
        assert!(f.catchup.on_page(req, p));
        assert!(!f.catchup.running());
        assert_eq!(queries(&f).len(), 1);
        assert_eq!(logs(&f, Level::Warn), ["catch-up: the recorder's call store answered: the call store is off"]);
        assert_eq!(f.catchup.store_error().as_deref(), Some("the call store is off"));

        // A later pass that ends cleanly clears it.
        assert!(f.catchup.begin(Mode::Start, NOW));
        let req = queries(&f)[1].0;
        assert_eq!(f.catchup.store_error().as_deref(), Some("the call store is off"), "kept while the pass runs");
        assert!(f.catchup.on_page(req, page(vec![], None)));
        assert_eq!(f.catchup.store_error(), None);
        assert_eq!(logs(&f, Level::Warn).len(), 1);
    }

    #[test]
    fn a_pass_queues_up_to_the_budget_and_the_next_periodic_one_walks_on() {
        let dir = testing::temp_dir("catchup");
        let mut f = blocked(&dir, 2);
        let entries: Vec<StoredCall> = (1..=5).map(|tg| convertible(&dir, tg).0).collect();
        assert!(f.catchup.begin(Mode::Start, NOW));
        let req = queries(&f)[0].0;
        assert!(f.catchup.on_page(req, page(entries, Some("500"))));
        assert_eq!(f.queue.catch_up_waiting(), 2, "the budget is 2");
        assert!(!f.catchup.running(), "the pass ended");
        assert_eq!(queries(&f).len(), 1, "no further page asked");
        assert_eq!(f.catchup.queued_total(), 2);
        let infos = logs(&f, Level::Info);
        assert!(infos.contains(&"catch-up paused: 2 calls queued, the rest waits for the next pass".to_string()), "{infos:?}");
        assert!(infos.contains(&"catch-up: 2 queued, 0 skipped, 0 already written, 1 pages".to_string()), "{infos:?}");

        // The next periodic pass goes on past a page with nothing queued...
        let b = written(&dir, 20);
        assert!(f.catchup.begin(Mode::Periodic, NOW));
        let req = queries(&f)[1].0;
        assert!(f.catchup.on_page(req, page(vec![b.clone()], Some("400"))));
        assert_eq!(queries(&f).len(), 3, "it asked for the next page");
        assert!(f.catchup.running());
        let req = queries(&f)[2].0;
        assert!(f.catchup.on_page(req, page(vec![], None)));
        assert!(!f.catchup.running());
        // ...and the one after stops early again.
        assert!(f.catchup.begin(Mode::Periodic, NOW));
        let req = queries(&f)[3].0;
        assert!(f.catchup.on_page(req, page(vec![b], Some("400"))));
        assert_eq!(queries(&f).len(), 4);
        assert!(!f.catchup.running());
        assert_eq!(f.catchup.queued_total(), 2);
        assert_eq!(f.seen.lock().unwrap().len(), 1, "only the saved call reached the blocked worker");
    }

    #[test]
    fn a_page_that_never_comes_ends_the_pass() {
        let dir = testing::temp_dir("catchup");
        let mut f = fixture(&dir);
        assert!(!f.catchup.check_timeout(Instant::now()), "no pass, nothing to end");
        assert!(f.catchup.begin(Mode::Start, NOW));
        assert!(!f.catchup.check_timeout(Instant::now() + Duration::from_secs(30)));
        assert!(f.catchup.running());
        assert!(f.catchup.check_timeout(Instant::now() + Duration::from_secs(61)));
        assert!(!f.catchup.running());
        assert_eq!(logs(&f, Level::Warn), ["catch-up: no answer from the recorder's call store in 60 s"]);
        assert!(logs(&f, Level::Info).contains(&"catch-up: 0 queued, 0 skipped, 0 already written, 0 pages".to_string()));
        let req = queries(&f)[0].0;
        let (late, _) = convertible(&dir, 6);
        assert!(!f.catchup.on_page(req, page(vec![late], None)), "a late page is not the pass's");
        assert!(f.queue.is_empty());
        assert!(f.seen.lock().unwrap().is_empty());
        assert!(f.catchup.begin(Mode::Start, NOW), "a new pass can start");
    }

    #[test]
    fn a_second_begin_while_running_is_refused_and_a_foreign_page_ignored() {
        let dir = testing::temp_dir("catchup");
        let mut f = fixture(&dir);
        assert!(f.catchup.begin(Mode::Start, NOW));
        assert!(!f.catchup.begin(Mode::Periodic, NOW));
        assert_eq!(queries(&f).len(), 1);
        let (x, _) = convertible(&dir, 7);
        assert!(!f.catchup.on_page(999, page(vec![x], None)));
        assert!(f.catchup.running());
        assert!(f.queue.is_empty());
        let req = queries(&f)[0].0;
        assert!(f.catchup.on_page(req, page(vec![], None)));
        assert!(!f.catchup.running());
        assert!(f.catchup.begin(Mode::Start, NOW));
    }
}
