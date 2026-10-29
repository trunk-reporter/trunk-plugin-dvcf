//! The work: one job, from a call's frames file to its `.dvcf`.
//!
//! First the skip rules (a disabled system, an analog or ProVoice call, a call with no frames file), then the frames file is read,
//! built into a `.dvcf` and written whole. A live or saved job overwrites a `.dvcf` already there; a catch-up job never does.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};
use std::time::Instant;

use trunk_recorder_plugin::{ConcludedCall, Setup};

use crate::config::{Config, SystemConfig};
use crate::dvcf::{Provenance, build};
use crate::frames::{find, read};
use crate::output::{target, write_atomic};
use crate::queue::{Done, Job, Kind};

/// The skip reason for a call with no frames file, which says how to get one.
pub const NO_FRAMES: &str = "no frames file: turn on captureFrames in Recording settings";

/// A system as the work sees it: its name, and whether its calls are written.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemView {
    pub short_name: String,
    pub enabled: bool,
}

/// The systems in `setup`; one with no settings of its own is enabled.
pub fn systems_of(setup: &Setup<Config, SystemConfig>) -> Vec<SystemView> {
    setup.systems.iter().map(|s| SystemView { short_name: s.short_name.clone(), enabled: s.config.as_ref().is_none_or(|c| c.enabled) }).collect()
}

/// What every job needs, shared by the worker, the catch-up and the plugin.
#[derive(Clone)]
pub struct Ctx {
    /// The plugin's settings (they change live).
    pub config: Arc<RwLock<Config>>,
    /// The systems and whether each is written (they change live).
    pub systems: Arc<RwLock<Vec<SystemView>>>,
    /// The recorder's capture folder.
    pub capture_dir: PathBuf,
    /// The recorder's version, for the file's provenance.
    pub host_version: String,
    /// How many calls were skipped, by reason.
    pub skipped: Arc<Mutex<BTreeMap<String, u64>>>,
}

/// A read lock, even on a lock a panic poisoned: the data behind it is plain settings.
fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// A mutex's guard, even on one a panic poisoned: the data behind it is plain counters.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why `call` is not written, if it is not: its system is disabled, it is analog or ProVoice, or it has no frames file (in that order).
/// A system not in the list counts as enabled.
pub fn skip_reason(ctx: &Ctx, call: &ConcludedCall) -> Option<String> {
    let record = &call.call;
    if read_lock(&ctx.systems).iter().any(|s| s.short_name == record.short_name && !s.enabled) {
        return Some("system disabled".into());
    }
    if record.audio_type == "analog" {
        return Some("analog".into());
    }
    if record.voice_mode == "provoice" {
        return Some("ProVoice".into());
    }
    if find(&call.files.json, &ctx.capture_dir, &call.path).is_none() {
        return Some(NO_FRAMES.into());
    }
    None
}

/// Counts one call skipped for `reason`.
pub fn count_skip(ctx: &Ctx, reason: &str) {
    *lock(&ctx.skipped).entry(reason.to_string()).or_default() += 1;
}

/// One try at `job`: the skip rules, then the frames file read, built and written. A catch-up job leaves a `.dvcf` already there
/// alone. A frames file that cannot be read, or a file that cannot be written, asks for a retry; each warning the build gives is passed
/// to `warn`, prefixed with the call's path.
pub fn process(ctx: &Ctx, job: &Job, warn: &dyn Fn(String)) -> Done {
    let call = &job.call;
    if let Some(reason) = skip_reason(ctx, call) {
        count_skip(ctx, &reason);
        return Done::Skipped(reason);
    }
    let output_dir = read_lock(&ctx.config).output_dir();
    let target = target(output_dir.as_deref(), &ctx.capture_dir, &call.path);
    if job.kind == Kind::CatchUp && target.exists() {
        return Done::Skipped("already written".into());
    }
    let Some((file, source)) = find(&call.files.json, &ctx.capture_dir, &call.path) else {
        // Gone since the skip rules looked.
        count_skip(ctx, NO_FRAMES);
        return Done::Skipped(NO_FRAMES.into());
    };
    let started = Instant::now();
    let frames = match read(&file, source) {
        Ok(frames) => frames,
        Err(e) => return Done::Retry(e),
    };
    let provenance = Provenance { host_version: &ctx.host_version, plugin_version: env!("CARGO_PKG_VERSION"), source };
    let built = match build(&call.call, &frames, provenance) {
        Ok(built) => built,
        Err(e) if e == "no frames" => {
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Done::Failed(format!("no frames in {name}"));
        }
        Err(e) => return Done::Failed(e),
    };
    for w in built.warnings {
        warn(format!("{}: {w}", call.path));
    }
    match write_atomic(&target, &built.bytes) {
        Ok(()) => Done::Ok { took: started.elapsed(), bytes: built.bytes.len() as u64 },
        Err(e) => Done::Retry(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use trunk_core::trunk::frames::Codec;
    use trunk_core::trunk::sdr::SdrMode;
    use trunk_recorder_plugin::testing;

    use crate::fixtures;

    fn ctx_for(dir: &Path, systems: Vec<SystemView>) -> Ctx {
        Ctx {
            config: Arc::new(RwLock::new(Config::default())),
            systems: Arc::new(RwLock::new(systems)),
            capture_dir: dir.to_path_buf(),
            host_version: "0.3.0".into(),
            skipped: Arc::default(),
        }
    }

    fn job(call: ConcludedCall, kind: Kind) -> Job {
        Job { call, kind, enqueued: Instant::now(), tries: 0 }
    }

    #[test]
    fn the_skip_rules_in_order() {
        let dir = testing::temp_dir("work");
        let ctx = ctx_for(&dir, vec![SystemView { short_name: "sys1".into(), enabled: false }]);
        let (call, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 30, 4);
        assert_eq!(skip_reason(&ctx, &call).as_deref(), Some("system disabled"));
        let ctx = ctx_for(&dir, vec![]);
        assert_eq!(skip_reason(&ctx, &call), None, "an unlisted system is enabled");
        let mut analog = call.clone();
        analog.call.audio_type = "analog".into();
        assert_eq!(skip_reason(&ctx, &analog).as_deref(), Some("analog"));
        let mut pv = call.clone();
        pv.call.voice_mode = "provoice".into();
        assert_eq!(skip_reason(&ctx, &pv).as_deref(), Some("ProVoice"));
        std::fs::remove_file(call.files.json.with_extension("sdr")).unwrap();
        assert_eq!(skip_reason(&ctx, &call).as_deref(), Some(NO_FRAMES));
    }

    #[test]
    fn a_live_job_writes_the_file_beside_the_call() {
        let dir = testing::temp_dir("work");
        let ctx = ctx_for(&dir, vec![]);
        let (call, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 31, 10);
        let done = process(&ctx, &job(call.clone(), Kind::Live), &|m| panic!("{m}"));
        let Done::Ok { bytes, .. } = done else { panic!("{done:?}") };
        let f = call.files.json.with_extension("dvcf");
        assert_eq!(std::fs::read(&f).unwrap().len() as u64, bytes);
        assert_eq!(crate::dvcf::read::messages(&std::fs::read(&f).unwrap()).len(), 13);
        // Live again: overwritten. Catch-up: left alone.
        std::fs::write(&f, b"old").unwrap();
        assert!(matches!(process(&ctx, &job(call.clone(), Kind::Live), &|_| {}), Done::Ok { .. }));
        assert_ne!(std::fs::read(&f).unwrap(), b"old");
        std::fs::write(&f, b"old").unwrap();
        assert_eq!(process(&ctx, &job(call.clone(), Kind::CatchUp), &|_| {}), Done::Skipped("already written".into()));
        assert_eq!(std::fs::read(&f).unwrap(), b"old");
    }

    #[test]
    fn a_spooled_call_is_written_under_the_capture_folder_not_in_the_spool() {
        let dir = testing::temp_dir("work");
        let spool = testing::temp_dir("spool");
        let ctx = ctx_for(&dir, vec![]);
        let (mut call, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 36, 5);
        // The call held in the recorder's RAM spool: its JSON and frames file are there, nothing under the capture folder yet.
        let json = spool.join("held.json");
        std::fs::copy(&call.files.json, &json).unwrap();
        std::fs::copy(call.files.json.with_extension("sdr"), spool.join("held.sdr")).unwrap();
        std::fs::remove_file(call.files.json.with_extension("sdr")).unwrap();
        std::fs::remove_file(&call.files.json).unwrap();
        call.files.json = json;
        let done = process(&ctx, &job(call.clone(), Kind::Live), &|m| panic!("{m}"));
        assert!(matches!(done, Done::Ok { .. }), "{done:?}");
        assert!(dir.join(format!("{}.dvcf", call.path)).is_file(), "at the call's key under the capture folder");
        let mut left: Vec<String> = std::fs::read_dir(&spool).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["held.json", "held.sdr"], "nothing written in the spool");
    }

    #[test]
    fn the_output_folder_mirrors_the_calls_layout() {
        let dir = testing::temp_dir("work");
        let out = testing::temp_dir("out");
        let ctx = ctx_for(&dir, vec![]);
        ctx.config.write().unwrap().output_dir = out.to_string_lossy().into_owned();
        let (call, _) = fixtures::sdr_call(&dir, SdrMode::Dmr, 32, 6);
        assert!(matches!(process(&ctx, &job(call.clone(), Kind::Live), &|_| {}), Done::Ok { .. }));
        assert!(out.join(format!("{}.dvcf", call.path)).exists());
        assert!(!call.files.json.with_extension("dvcf").exists());
    }

    #[test]
    fn empty_frames_fail_and_a_warning_names_the_call() {
        let dir = testing::temp_dir("work");
        let ctx = ctx_for(&dir, vec![]);
        let call = fixtures::jsonl_call(&dir, 33, &[]);
        assert!(matches!(process(&ctx, &job(call.clone(), Kind::Live), &|_| {}), Done::Failed(m) if m.starts_with("no frames in ")));
        let mut frames = fixtures::frames(Codec::Ambe, 2);
        frames.extend(fixtures::frames(Codec::Imbe, 1));
        let mut odd = fixtures::jsonl_call(&dir, 34, &frames);
        odd.call.voice_mode = "p25".into();
        let warned = std::cell::RefCell::new(Vec::new());
        assert!(matches!(process(&ctx, &job(odd.clone(), Kind::Live), &|m| warned.borrow_mut().push(m)), Done::Ok { .. }));
        assert_eq!(warned.borrow().as_slice(), [format!("{}: AMBE frames under voice mode \"p25\": written as P25 Phase 2 (type 1)", odd.path)]);
        assert_eq!(ctx.skipped.lock().unwrap().len(), 0);
    }

    #[test]
    fn an_unwritable_target_asks_for_a_retry() {
        let dir = testing::temp_dir("work");
        let ctx = ctx_for(&dir, vec![]);
        let (call, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 35, 3);
        std::fs::create_dir_all(call.files.json.with_extension("dvcf")).unwrap(); // a folder where the file should go
        assert!(matches!(process(&ctx, &job(call, Kind::Live), &|_| {}), Done::Retry(m) if m.starts_with("cannot write ")));
    }
}
