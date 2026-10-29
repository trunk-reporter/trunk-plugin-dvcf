//! The plugin: wires the queue, the work, the catch-up pass, the saved queue and the retention sweep together; takes new
//! settings while it runs (every setting is live); keeps its metrics and status current; and shuts down inside the grace,
//! saving the live calls not yet written for the next start.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use trunk_recorder_plugin::{CallsPage, ConcludedCall, Host, Manifest, Metrics, Outcome, Plugin, Setup, State, topic};

use crate::catchup::{BUDGET, CatchUp, Mode};
use crate::config::{Config, SystemConfig};
use crate::frames::find;
use crate::output::sweep;
use crate::queue::{Kind, Queue, QueueOptions, Snapshot};
use crate::saved;
use crate::work::{Ctx, NO_FRAMES, count_skip, process, skip_reason, systems_of};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The upkeep thread's beat.
const TICK: Duration = Duration::from_secs(10);
/// The time between periodic catch-up passes.
const CATCH_UP_EVERY: Duration = Duration::from_secs(600);
/// The first retention sweep, and the time between sweeps.
const RETAIN_FIRST: Duration = Duration::from_secs(60);
const RETAIN_EVERY: Duration = Duration::from_secs(3600);
/// The waits before each retry of a job that could not be done just now.
const RETRY_AFTER: [Duration; 3] = [Duration::from_secs(1), Duration::from_secs(5), Duration::from_secs(15)];
const DAY_S: u64 = 86_400;

pub struct Dvcf {
    host: Host,
    ctx: Ctx,
    queue: Arc<Queue>,
    catchup: Arc<Mutex<CatchUp>>,
    report: Arc<Report>,
    upkeep: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
    /// Where the saved queue goes at shutdown.
    data_dir: PathBuf,
    started: Instant,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Makes the output folder when `config` names one; why it could not, if it could not.
fn make_output_dir(config: &Config) -> Option<String> {
    let dir = config.output_dir()?;
    std::fs::create_dir_all(&dir).err().map(|e| format!("cannot create the output folder {}: {e}", dir.display()))
}

/// Where the files go, as the status and the start line say it.
fn where_of(config: &Config) -> String {
    match config.output_dir() {
        None => "writing beside the calls".into(),
        Some(dir) => format!("writing to {}", dir.display()),
    }
}

impl Plugin for Dvcf {
    type Config = Config;
    type SystemConfig = SystemConfig;

    fn manifest() -> Manifest {
        Manifest {
            name: "DVCF".into(),
            subscribe: vec![topic::CALL_CONCLUDED.into()],
            audio_formats: vec![],
            live_settings: true,
            ..trunk_recorder_plugin::manifest!()
        }
    }

    fn start(host: Host, setup: Setup<Config, SystemConfig>) -> Result<Self, String> {
        let config = setup.config.clone();
        config.validate()?;
        // A folder that cannot be made is the status, not a refusal: each call then fails on its own.
        let output_error = make_output_dir(&config);
        let ctx = Ctx {
            config: Arc::new(RwLock::new(config.clone())),
            systems: Arc::new(RwLock::new(systems_of(&setup))),
            capture_dir: setup.capture_dir.clone(),
            host_version: setup.host.version.clone(),
            skipped: Arc::default(),
            write_error: Arc::default(),
            failed: Arc::default(),
        };
        let queue = {
            let (work_ctx, work_host) = (ctx.clone(), host.clone());
            let opts = QueueOptions { retry_after: RETRY_AFTER.to_vec() };
            Arc::new(Queue::start(host.clone(), opts, move |job| process(&work_ctx, job, &|m| work_host.warn(m))))
        };
        let catchup = Arc::new(Mutex::new(CatchUp::new(host.clone(), ctx.clone(), queue.clone(), BUDGET)));

        // The live calls the last shutdown saved go first, ahead of any catch-up.
        for call in saved::load(&setup.data_dir, &|m| host.warn(m)) {
            if find(&call.files.json, &ctx.capture_dir, &call.path).is_some() {
                queue.push(call, Kind::Saved);
            } else {
                host.info(format!("saved call {} dropped: its frames file is gone", call.path));
            }
        }
        host.info(format!("dvcf {VERSION}: {}", where_of(&config)));
        if config.catch_up {
            lock(&catchup).begin(Mode::Start, now_ms());
        }

        let started = Instant::now();
        let report = Arc::new(Report {
            host: host.clone(),
            ctx: ctx.clone(),
            queue: queue.clone(),
            catchup: catchup.clone(),
            refused: Mutex::new(None),
            output_error: Mutex::new(output_error),
            no_frames: AtomicBool::new(false),
            closed: Mutex::new(false),
        });
        let mut upkeep = Upkeep { report: report.clone(), next_catch_up: started + CATCH_UP_EVERY, next_retain: started + RETAIN_FIRST };
        let (stop, stopped) = mpsc::channel::<()>();
        let spawned = thread::Builder::new().name("upkeep".into()).spawn(move || {
            loop {
                upkeep.tick(Instant::now());
                match stopped.recv_timeout(TICK) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                // The saved calls were taken off the disk above: put them back before giving up.
                let calls = queue.shutdown(Duration::ZERO);
                if let Err(e) = saved::save(&setup.data_dir, &calls) {
                    host.warn(e);
                }
                return Err(format!("cannot start the upkeep thread: {e}"));
            }
        };
        Ok(Dvcf { host, ctx, queue, catchup, report, upkeep: Some((stop, thread)), data_dir: setup.data_dir, started })
    }

    fn call_concluded(&mut self, call: ConcludedCall) {
        if let Some(reason) = skip_reason(&self.ctx, &call) {
            count_skip(&self.ctx, &reason);
            // The warning shows from the first call with no frames file until a call with one comes.
            if reason == NO_FRAMES && !self.report.no_frames.swap(true, Ordering::Relaxed) {
                self.report.send_status();
            }
            self.host.call_result(&call.path, Outcome::Skipped, reason, "");
            return;
        }
        if self.report.no_frames.swap(false, Ordering::Relaxed) {
            self.report.send_status();
        }
        let path = call.path.clone();
        if !self.queue.push(call, Kind::Live) {
            self.host.call_result(&path, Outcome::Skipped, "already queued", "");
        }
    }

    fn calls(&mut self, request: u64, page: CallsPage) {
        lock(&self.catchup).on_page(request, page);
    }

    fn settings(&mut self, setup: Setup<Config, SystemConfig>) -> Result<(), String> {
        if let Err(e) = setup.config.validate() {
            // Kept as the status (the SDK shows it now; the upkeep keeps showing it) until good settings come.
            *lock(&self.report.refused) = Some(format!("new settings not taken: {e}"));
            return Err(e);
        }
        // Tried with every accepted settings, not only a changed folder, so the error clears once the folder can be made.
        *lock(&self.report.output_error) = make_output_dir(&setup.config);
        *write(&self.ctx.systems) = systems_of(&setup);
        *write(&self.ctx.config) = setup.config;
        *lock(&self.report.refused) = None;
        Ok(())
    }

    fn shutdown(&mut self, grace: Duration) {
        self.host.debug(format!("stopping after {} s, {grace:?} to do it in", self.started.elapsed().as_secs()));
        // Everything below ends inside 0.9 x grace, so the recorder never has to kill the process.
        let t0 = Instant::now();
        let deadline = t0 + grace.mul_f64(0.9);
        let remaining = || deadline.saturating_duration_since(Instant::now());
        // The upkeep sends nothing more and starts no pass or sweep; it is told to stop and given a moment to end its
        // tick. A sweep still walking after that is left behind (detached): the gate keeps it quiet.
        self.report.close();
        if let Some((stop, thread)) = self.upkeep.take() {
            let _ = stop.send(());
            let join_by = (t0 + grace.mul_f64(0.1)).min(deadline);
            while !thread.is_finished() && Instant::now() < join_by {
                thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
        // Half the grace for the live and saved work to end on its own (catch-up work is not waited for: the next pass finds
        // it again); the queue's shutdown then takes at most 0.8 of what is left.
        let idle_by = t0 + grace.mul_f64(0.5);
        while self.queue.front_pending() > 0 && Instant::now() < idle_by {
            thread::sleep(Duration::from_millis(20));
        }
        let calls = self.queue.shutdown(remaining());
        if let Err(e) = saved::save(&self.data_dir, &calls) {
            self.host.warn(e);
        }
        if !calls.is_empty() {
            self.host.info(format!("{} live calls saved for the next start", calls.len()));
        }
        self.report.send_last();
    }
}

/// Metrics and status, made from the queue's snapshot and what the plugin remembers.
struct Report {
    host: Host,
    ctx: Ctx,
    queue: Arc<Queue>,
    catchup: Arc<Mutex<CatchUp>>,
    /// Why the last settings were refused: the status until good ones come.
    refused: Mutex<Option<String>>,
    /// Why the output folder could not be made, at start or with new settings.
    output_error: Mutex<Option<String>>,
    /// The last live call that was looked at had no frames file.
    no_frames: AtomicBool,
    /// Shutdown has begun: nothing but shutdown's own last report is sent. Held across each send, so the check and the
    /// send are one step and nothing goes out after the last report.
    closed: Mutex<bool>,
}

impl Report {
    /// Metrics, then status; nothing once closed.
    fn send(&self) {
        let closed = lock(&self.closed);
        if !*closed {
            self.metrics_now();
            self.status_now();
        }
    }

    /// The status alone; nothing once closed.
    fn send_status(&self) {
        let closed = lock(&self.closed);
        if !*closed {
            self.status_now();
        }
    }

    fn is_closed(&self) -> bool {
        *lock(&self.closed)
    }

    /// Closes the gate: from now on only [`Report::send_last`] sends.
    fn close(&self) {
        *lock(&self.closed) = true;
    }

    /// Shutdown's last metrics and status, through the same gate.
    fn send_last(&self) {
        let mut closed = lock(&self.closed);
        *closed = true;
        self.metrics_now();
        self.status_now();
    }

    fn metrics_now(&self) {
        let snap = self.queue.snapshot();
        let catch_up_queued = lock(&self.catchup).queued_total();
        self.host.metrics(&metrics_of(&snap, &self.ctx, catch_up_queued));
    }

    fn status_now(&self) {
        let store_error = lock(&self.catchup).store_error();
        let output_error = lock(&self.output_error).clone();
        let write_error = lock(&self.ctx.write_error).clone();
        let where_ = where_of(&read(&self.ctx.config));
        let no_frames = self.no_frames.load(Ordering::Relaxed);
        // Held while sending (nothing the host does takes it), so a refusal recorded by `settings` is never overwritten by a stale Ok.
        let refused = lock(&self.refused);
        let (state, message) = status_of(refused.as_deref(), output_error.as_deref(), write_error.as_deref(), no_frames, store_error.as_deref(), &where_);
        self.host.status(state, message);
    }
}

/// The metrics the recorder shows: the queue's figures, and the run's totals in `extra`.
fn metrics_of(snap: &Snapshot, ctx: &Ctx, catch_up_queued: u64) -> Metrics {
    let skipped = lock(&ctx.skipped).clone();
    let extra = BTreeMap::from([
        ("written".to_string(), json!(snap.ok)),
        ("bytes_written".to_string(), json!(snap.bytes)),
        ("failed".to_string(), json!(snap.failed)),
        ("skipped".to_string(), json!(skipped)),
        ("catch_up_queued".to_string(), json!(catch_up_queued)),
        ("catch_up_written".to_string(), json!(snap.catch_up_ok)),
    ]);
    Metrics {
        queued: Some(snap.queued as u64),
        retrying: Some(snap.retrying as u64),
        in_flight: Some(snap.in_flight as u64),
        retries: Some(snap.retries),
        latency_ms: snap.latency_ms,
        last_ok: snap.last_ok_s.map(|s| s as f64),
        last_error: snap.last_error_s.map(|s| s as f64),
        last_error_text: snap.last_error.clone(),
        extra,
        ..Default::default()
    }
}

/// The status, first rule first: refused settings, an output folder that cannot be made and a write that failed (until the next
/// one that works) are errors; calls with no frames file and a call store that answered an error are warnings; else ok, saying
/// where the files go.
fn status_of(
    refused: Option<&str>,
    output_error: Option<&str>,
    write_error: Option<&str>,
    no_frames: bool,
    store_error: Option<&str>,
    where_: &str,
) -> (State, String) {
    if let Some(m) = refused {
        (State::Error, m.to_string())
    } else if let Some(m) = output_error {
        (State::Error, m.to_string())
    } else if let Some(m) = write_error {
        (State::Error, m.to_string())
    } else if no_frames {
        (State::Warning, NO_FRAMES.to_string())
    } else if let Some(e) = store_error {
        (State::Warning, format!("call store: {e}"))
    } else {
        (State::Ok, where_.to_string())
    }
}

/// The upkeep thread's state: what is due when.
struct Upkeep {
    report: Arc<Report>,
    next_catch_up: Instant,
    next_retain: Instant,
}

impl Upkeep {
    /// One beat: the catch-up's page timeout, the periodic catch-up and the retention sweep when due, then metrics and status.
    /// Once shutdown has begun it does nothing: no pass, no sweep, no report.
    fn tick(&mut self, now: Instant) {
        if self.report.is_closed() {
            return;
        }
        let (ctx, host) = (&self.report.ctx, &self.report.host);
        lock(&self.report.catchup).check_timeout(now);
        let (catch_up, retention_days, output_dir) = {
            let c = read(&ctx.config);
            (c.catch_up, c.retention_days, c.output_dir())
        };
        if now >= self.next_catch_up {
            self.next_catch_up = now + CATCH_UP_EVERY;
            if catch_up && !self.report.is_closed() {
                lock(&self.report.catchup).begin(Mode::Periodic, now_ms());
            }
        }
        if now >= self.next_retain {
            self.next_retain = now + RETAIN_EVERY;
            if retention_days > 0 && !self.report.is_closed() {
                let root = output_dir.unwrap_or_else(|| ctx.capture_dir.clone());
                let n = sweep(&root, Duration::from_secs(u64::from(retention_days) * DAY_S), SystemTime::now(), &|m| host.warn(m));
                if n > 0 {
                    host.info(format!("{n} .dvcf files older than {retention_days} days deleted"));
                }
            }
        }
        self.report.send();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use trunk_core::trunk::sdr::SdrMode;
    use trunk_recorder_plugin::testing;
    use trunk_recorder_plugin::{CallsPage, HostInfo, HostMessage, Level, Outcome, PluginMessage, State, StoredCall, SystemInfo};

    use crate::fixtures;
    use crate::work::NO_FRAMES;

    /// Polls `cond` until it holds, failing the test after 10 s.
    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn statuses(out: &testing::Output) -> Vec<(State, String)> {
        out.messages
            .iter()
            .filter_map(|m| match m {
                PluginMessage::Status { state, message } => Some((*state, message.clone())),
                _ => None,
            })
            .collect()
    }

    fn calls_queries(out: &testing::Output) -> usize {
        out.messages.iter().filter(|m| matches!(m, PluginMessage::CallsQuery { .. })).count()
    }

    fn has_info(out: &testing::Output, text: &str) -> bool {
        out.logs().iter().any(|(l, m)| *l == Level::Info && m == text)
    }

    /// A setup by hand, as the SDK makes it from a hello, with no systems listed (so every system is enabled).
    fn setup(dir: &Path, config: Config) -> Setup<Config, SystemConfig> {
        std::fs::create_dir_all(dir.join("data")).unwrap();
        Setup {
            config,
            systems: Vec::new(),
            host: HostInfo::default(),
            capture_dir: dir.to_path_buf(),
            data_dir: dir.join("data"),
            audio_formats: vec!["wav".into()],
        }
    }

    #[test]
    fn the_manifest_is_dvcf() {
        let m = trunk_recorder_plugin::describe::<Dvcf>();
        assert_eq!((m.id.as_str(), m.name.as_str(), m.api), ("dvcf", "DVCF", 1));
        assert_eq!(m.subscribe, ["call.concluded"]);
        assert!(m.audio_formats.is_empty() && m.live_settings && m.config.is_some() && m.system_config.is_some());
        assert_eq!(m.license, "GPL-3.0-or-later");
    }

    #[test]
    fn bad_settings_stop_it_and_good_ones_start_it() {
        let dir = testing::temp_dir("dvcf");
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "outputDir": "relative" })))]);
        assert_eq!(out.exit_code, trunk_recorder_plugin::EXIT_CONFIG);
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({})))]);
        assert!(out.ready());
    }

    #[test]
    fn a_live_call_is_written_and_answered() {
        let dir = testing::temp_dir("plugin");
        let (c, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 101, 10);
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({}))), HostMessage::CallConcluded(c.clone())]);
        assert!(out.ready());
        assert_eq!(out.results(), [(c.path.clone(), Outcome::Ok, String::new(), String::new())]);
        let took = out.messages.iter().find_map(|m| match m {
            PluginMessage::CallResult { took_ms, .. } => Some(*took_ms),
            _ => None,
        });
        assert!(matches!(took, Some(Some(_))), "the result carries took_ms: {took:?}");
        let file = c.files.json.with_extension("dvcf");
        assert!(!crate::dvcf::read::messages(&std::fs::read(&file).unwrap()).is_empty());
        assert_eq!(calls_queries(&out), 1, "the start pass asked for calls once");
        assert!(has_info(&out, &format!("dvcf {VERSION}: writing beside the calls")), "{:?}", out.logs());
        assert_eq!(out.status(), Some((State::Ok, "writing beside the calls".to_string())));
        assert_eq!(out.metrics().last().expect("metrics").extra["written"], json!(1));
    }

    #[test]
    fn calls_without_frames_are_skipped_and_the_status_says_why() {
        let dir = testing::temp_dir("plugin");
        // sys1 as testing::hello has it, and a second system, sys2, turned off.
        let mut hello = testing::hello(&dir, json!({}));
        hello.systems.push(SystemInfo { index: 1, short_name: "sys2".into(), kind: "p25".into(), config: json!({ "enabled": false }), ..Default::default() });
        let bare = testing::call(&dir, "sys1", 110);
        let (good, _) = fixtures::sdr_call(&dir, SdrMode::Dmr, 111, 6);
        let mut analog = testing::call(&dir, "sys1", 112);
        analog.call.audio_type = "analog".into();
        let disabled = testing::call(&dir, "sys2", 113);
        let out = testing::run::<Dvcf>([
            HostMessage::Hello(hello),
            HostMessage::CallConcluded(bare.clone()),
            HostMessage::CallConcluded(analog.clone()),
            HostMessage::CallConcluded(disabled.clone()),
            HostMessage::CallConcluded(good.clone()),
        ]);
        let results: Vec<_> = out.results().into_iter().map(|r| (r.0, r.1, r.2)).collect();
        assert_eq!(
            results,
            [
                (bare.path.clone(), Outcome::Skipped, NO_FRAMES.to_string()),
                (analog.path.clone(), Outcome::Skipped, "analog".to_string()),
                (disabled.path.clone(), Outcome::Skipped, "system disabled".to_string()),
                (good.path.clone(), Outcome::Ok, String::new()),
            ]
        );
        let statuses = statuses(&out);
        let warned = statuses.iter().position(|s| *s == (State::Warning, NO_FRAMES.to_string())).expect("a warning for the call with no frames file");
        assert!(statuses[warned..].iter().any(|s| s.0 == State::Ok), "back to ok once a call with frames came: {statuses:?}");
        assert_eq!(out.status(), Some((State::Ok, "writing beside the calls".to_string())));
        let skipped = out.metrics().last().expect("metrics").extra["skipped"].clone();
        assert_eq!(skipped, json!({ "analog": 1, "system disabled": 1, NO_FRAMES: 1 }));

        // A call kept with no audio (keepEncrypted / keepSilent) has no frames file either, but it is not a captureFrames problem.
        let mut silent = testing::call(&dir, "sys1", 114);
        (silent.call.call_length_ms, silent.call.call_length) = (0, 0.0);
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "catchUp": false }))), HostMessage::CallConcluded(silent.clone())]);
        let results: Vec<_> = out.results().into_iter().map(|r| (r.0, r.1, r.2)).collect();
        assert_eq!(results, [(silent.path.clone(), Outcome::Skipped, "no audio".to_string())]);
        let after = self::statuses(&out);
        assert!(!after.is_empty() && after.iter().all(|s| s.0 == State::Ok), "no warning: {after:?}");
        assert_eq!(out.metrics().last().expect("metrics").extra["skipped"], json!({ "no audio": 1 }));
    }

    #[test]
    fn a_call_concluded_twice_gets_one_answer_each() {
        let dir = testing::temp_dir("plugin");
        let (c, _) = fixtures::sdr_call(&dir, SdrMode::P25Tdma, 120, 8);
        let out = testing::run::<Dvcf>([
            HostMessage::Hello(testing::hello(&dir, json!({}))),
            HostMessage::CallConcluded(c.clone()),
            HostMessage::CallConcluded(c.clone()),
        ]);
        let results = out.results();
        assert_eq!(results.len(), 2, "{results:?}");
        for (path, outcome, message, _) in &results {
            assert_eq!(path, &c.path);
            let ok = *outcome == Outcome::Ok && message.is_empty();
            let refused = *outcome == Outcome::Skipped && message == "already queued";
            assert!(ok || refused, "{results:?}");
        }
        assert!(c.files.json.with_extension("dvcf").is_file());
    }

    #[test]
    fn a_page_from_the_store_is_converted_without_a_result() {
        let dir = testing::temp_dir("plugin");
        let (c, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 130, 5);
        let stored = StoredCall { path: c.path.clone(), record: c.call.clone(), audio: true, json: true };
        let page = CallsPage { entries: vec![stored], next: None, error: None };
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "catchUp": true }))), HostMessage::Calls { request: 1, page }]);
        assert!(out.ready());
        assert!(c.files.json.with_extension("dvcf").is_file(), "the catch-up wrote it");
        assert!(out.results().is_empty(), "a catch-up call sends no call.result");
        assert!(out.logs().iter().any(|(l, m)| *l == Level::Info && m.contains("catch-up: 1 queued")), "{:?}", out.logs());
        assert_eq!(out.metrics().last().expect("metrics").extra["catch_up_queued"], json!(1));
    }

    #[test]
    fn catch_up_off_asks_for_nothing() {
        let dir = testing::temp_dir("plugin");
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "catchUp": false })))]);
        assert!(out.ready());
        assert_eq!(calls_queries(&out), 0);
    }

    #[test]
    fn new_settings_are_taken_at_once() {
        let dir = testing::temp_dir("plugin");
        let target = testing::temp_dir("out").join("dvcf");
        let (c, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 140, 4);
        let out = testing::run::<Dvcf>([
            HostMessage::Hello(testing::hello(&dir, json!({}))),
            HostMessage::Settings(testing::hello(&dir, json!({ "outputDir": target.to_string_lossy() }))),
            HostMessage::CallConcluded(c.clone()),
            HostMessage::Settings(testing::hello(&dir, json!({ "outputDir": "relative" }))),
        ]);
        assert_eq!(out.results().iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(), [(c.path.as_str(), Outcome::Ok)]);
        assert!(target.join(format!("{}.dvcf", c.path)).is_file(), "written under the new output folder");
        assert!(!c.files.json.with_extension("dvcf").exists(), "not beside the call");
        assert_eq!(out.status(), Some((State::Error, "new settings not taken: Output folder: must be an absolute path".to_string())));

        // Settings naming a folder that cannot be made yet: an error, cleared by the same settings once it can be made.
        let file = dir.join("not-yet-a-folder");
        std::fs::write(&file, b"x").unwrap();
        let later = file.join("dvcf");
        let (host, captured) = testing::capture();
        let mut plugin = Dvcf::start(host, setup(&dir, Config { catch_up: false, ..Default::default() })).unwrap();
        let same = || setup(&dir, Config { catch_up: false, output_dir: later.to_string_lossy().into_owned(), ..Default::default() });
        plugin.settings(same()).unwrap();
        plugin.report.send_status();
        let (state, message) = captured.output().status().expect("a status");
        assert_eq!(state, State::Error);
        assert!(message.starts_with("cannot create the output folder "), "{message}");
        std::fs::remove_file(&file).unwrap();
        plugin.settings(same()).unwrap();
        plugin.report.send_status();
        assert_eq!(captured.output().status(), Some((State::Ok, format!("writing to {}", later.display()))));
        assert!(later.is_dir());
        plugin.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn an_output_folder_that_cannot_be_made_is_an_error_but_it_runs() {
        let dir = testing::temp_dir("plugin");
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let blocked = file.join("dvcf");
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "outputDir": blocked.to_string_lossy(), "catchUp": false })))]);
        assert!(out.ready(), "it still starts");
        let (state, message) = out.status().expect("a status");
        assert_eq!(state, State::Error);
        assert!(message.starts_with(&format!("cannot create the output folder {}: ", blocked.display())), "{message}");
    }

    #[test]
    fn shutdown_saves_the_queue_and_the_next_start_works_and_answers_it() {
        let dir = testing::temp_dir("plugin");
        let calls: Vec<ConcludedCall> = (1..=3).map(|k| fixtures::sdr_call(&dir, SdrMode::P25Fdma, 150 + k, 4).0).collect();
        // A folder where call 1's file should go: its write fails and it waits for a retry (1 s).
        let blocker = calls[0].files.json.with_extension("dvcf");
        std::fs::create_dir_all(&blocker).unwrap();
        let (host, captured) = testing::capture();
        let mut plugin = Dvcf::start(host, setup(&dir, Config { catch_up: false, ..Default::default() })).unwrap();
        for c in &calls {
            plugin.call_concluded(c.clone());
        }
        let oks = || captured.output().results().iter().filter(|r| r.1 == Outcome::Ok).count();
        wait_for("calls 2 and 3 written", || oks() == 2);
        let t0 = Instant::now();
        plugin.shutdown(Duration::from_secs(1));
        assert!(t0.elapsed() < Duration::from_millis(900), "took {:?} of a 1 s grace", t0.elapsed());

        let out = captured.output();
        let answered: Vec<_> = out.results().into_iter().map(|r| (r.0, r.1)).collect();
        assert_eq!(answered, [(calls[1].path.clone(), Outcome::Ok), (calls[2].path.clone(), Outcome::Ok)], "nothing for call 1");
        assert!(has_info(&out, "1 live calls saved for the next start"), "{:?}", out.logs());
        let text = std::fs::read_to_string(dir.join("data").join(crate::saved::FILE)).expect("the saved queue");
        let saved: Vec<ConcludedCall> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(saved.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), [calls[0].path.as_str()]);

        std::fs::remove_dir(&blocker).unwrap();
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "catchUp": false })))]);
        assert!(out.ready());
        assert!(!dir.join("data").join(crate::saved::FILE).exists(), "taken once");
        assert!(blocker.is_file(), "call 1 written at the next start");
        let answered: Vec<_> = out.results().into_iter().map(|r| (r.0, r.1)).collect();
        assert_eq!(answered, [(calls[0].path.clone(), Outcome::Ok)], "a saved call is answered when it is worked");
    }

    #[test]
    fn shutdown_does_not_spend_its_grace_on_a_catch_up_backlog() {
        let dir = testing::temp_dir("plugin");
        let (host, captured) = testing::capture();
        let mut plugin = Dvcf::start(host.clone(), setup(&dir, Config { catch_up: false, ..Default::default() })).unwrap();
        // A free worker that takes ~100 ms a job, and a backlog of catch-up work.
        let seen = Arc::new(Mutex::new(0usize));
        let s = seen.clone();
        let queue = Arc::new(Queue::start(host, QueueOptions { retry_after: vec![] }, move |_| {
            *lock(&s) += 1;
            thread::sleep(Duration::from_millis(100));
            crate::queue::Done::Ok { took: Duration::ZERO, bytes: 0 }
        }));
        for k in 1..=20 {
            assert!(queue.push(testing::call(&dir, "sys1", 170 + k), Kind::CatchUp));
        }
        wait_for("the first job to start", || *lock(&seen) > 0);
        plugin.queue = queue;
        let t0 = Instant::now();
        plugin.shutdown(Duration::from_secs(2));
        let took = t0.elapsed();
        // Waiting for the queue to empty, the old loop would have worked ~10 of them in its 1 s.
        assert!(*lock(&seen) <= 2, "{} jobs worked: the one in flight, at most one more", *lock(&seen));
        assert!(took < Duration::from_millis(600), "took {took:?} of a 2 s grace");
        assert!(!dir.join("data").join(crate::saved::FILE).exists(), "no catch-up call is saved");
        assert!(!captured.output().logs().iter().any(|(_, m)| m.contains("saved for the next start")), "{:?}", captured.output().logs());
    }

    #[test]
    fn a_saved_call_whose_frames_file_is_gone_is_dropped() {
        let dir = testing::temp_dir("plugin");
        let (c, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 160, 4);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        crate::saved::save(&dir.join("data"), std::slice::from_ref(&c)).unwrap();
        std::fs::remove_file(c.files.json.with_extension("sdr")).unwrap();
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "catchUp": false })))]);
        assert!(has_info(&out, &format!("saved call {} dropped: its frames file is gone", c.path)), "{:?}", out.logs());
        assert!(out.results().is_empty());
    }

    #[test]
    fn retention_sweeps_only_when_set() {
        let dir = testing::temp_dir("plugin");
        let (host, captured) = testing::capture();
        let mut plugin = Dvcf::start(host, setup(&dir, Config { catch_up: false, retention_days: 7, ..Default::default() })).unwrap();
        let mk = |rel: &str, old: bool| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
            if old {
                let t = SystemTime::now() - Duration::from_secs(8 * 86_400);
                std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
            }
            p
        };
        let old = mk("sys1/2026/10/20/old.dvcf", true);
        let fresh = mk("sys1/2026/10/29/fresh.dvcf", false);
        let now = Instant::now();
        let upkeep = |retain_at: Instant| Upkeep { report: plugin.report.clone(), next_catch_up: now + CATCH_UP_EVERY, next_retain: retain_at };
        let mut u = upkeep(now - Duration::from_secs(1));
        u.tick(now);
        assert!(!old.exists() && fresh.exists());
        assert!(has_info(&captured.output(), "1 .dvcf files older than 7 days deleted"), "{:?}", captured.output().logs());
        assert!(u.next_retain >= now + RETAIN_EVERY, "the next sweep is an hour away");

        write(&plugin.ctx.config).retention_days = 0;
        let old = mk("sys1/2026/10/20/old.dvcf", true);
        upkeep(now - Duration::from_secs(1)).tick(now);
        assert!(old.exists() && fresh.exists(), "0 keeps everything");
        plugin.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn after_shutdown_reports_last_nothing_more_is_sent_or_started() {
        let dir = testing::temp_dir("plugin");
        let (host, captured) = testing::capture();
        let mut plugin = Dvcf::start(host, setup(&dir, Config { catch_up: true, retention_days: 7, ..Default::default() })).unwrap();
        let old = dir.join("sys1/2026/10/20/old.dvcf");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"x").unwrap();
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(SystemTime::now() - Duration::from_secs(8 * 86_400)).unwrap();
        let t0 = Instant::now();
        plugin.shutdown(Duration::from_secs(1));
        assert!(t0.elapsed() < Duration::from_millis(900), "took {:?} of a 1 s grace", t0.elapsed());
        let out = captured.output();
        let n = out.messages.len();
        assert!(matches!(&out.messages[n - 2..], [PluginMessage::Metrics(_), PluginMessage::Status { .. }]), "shutdown's report is last: {:?}", out.messages);

        // A tick that was under way when shutdown came, with a periodic pass and a sweep both due, and any other send.
        let now = Instant::now();
        let mut late = Upkeep { report: plugin.report.clone(), next_catch_up: now - Duration::from_secs(1), next_retain: now - Duration::from_secs(1) };
        late.tick(now);
        plugin.report.send();
        plugin.report.send_status();
        assert_eq!(captured.output().messages.len(), n, "nothing after the last report: {:?}", &captured.output().messages[n..]);
        assert_eq!(calls_queries(&captured.output()), 1, "only the start pass asked for calls");
        assert!(old.exists(), "no sweep once shutdown has begun");
    }

    #[test]
    fn the_status_rules_in_order() {
        let place = "writing beside the calls";
        assert_eq!(status_of(None, None, None, false, None, place), (State::Ok, place.to_string()));
        assert_eq!(status_of(None, None, None, false, Some("store off"), place), (State::Warning, "call store: store off".to_string()));
        assert_eq!(status_of(None, None, None, true, Some("store off"), place), (State::Warning, NO_FRAMES.to_string()));
        assert_eq!(status_of(None, None, Some("cannot write"), true, Some("store off"), place), (State::Error, "cannot write".to_string()));
        assert_eq!(status_of(None, Some("cannot create"), Some("cannot write"), true, Some("store off"), place), (State::Error, "cannot create".to_string()));
        let refused = status_of(Some("new settings not taken: x"), Some("cannot create"), Some("cannot write"), true, Some("s"), place);
        assert_eq!(refused, (State::Error, "new settings not taken: x".to_string()));
    }

    #[test]
    fn the_metrics_carry_the_run_totals() {
        let dir = testing::temp_dir("plugin");
        let ctx = Ctx {
            config: Arc::default(),
            systems: Arc::default(),
            capture_dir: dir,
            host_version: "0".into(),
            skipped: Arc::new(Mutex::new(BTreeMap::from([("analog".to_string(), 2)]))),
            write_error: Arc::default(),
            failed: Arc::default(),
        };
        let snap = Snapshot {
            queued: 3,
            retrying: 1,
            in_flight: 1,
            retries: 4,
            ok: 9,
            failed: 2,
            catch_up_ok: 5,
            bytes: 1000,
            last_error: "disk".into(),
            ..Default::default()
        };
        let m = metrics_of(&snap, &ctx, 7);
        assert_eq!((m.queued, m.retrying, m.in_flight, m.retries, m.last_error_text.as_str()), (Some(3), Some(1), Some(1), Some(4), "disk"));
        let extra = json!(m.extra);
        assert_eq!(extra, json!({ "written": 9, "bytes_written": 1000, "failed": 2, "skipped": { "analog": 2 }, "catch_up_queued": 7, "catch_up_written": 5 }));
    }
}
