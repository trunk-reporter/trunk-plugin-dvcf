//! Output: where a call's .dvcf file goes, how it is written whole, and how old ones are swept away.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Where a call's .dvcf goes: beside its JSON when there is no output folder, otherwise under the folder at the call's own `system/Y/M/D/base` key.
pub fn target(output_dir: Option<&Path>, json: &Path, path: &str) -> PathBuf {
    match output_dir {
        None => json.with_extension("dvcf"),
        Some(dir) => dir.join(format!("{path}.dvcf")),
    }
}

/// Writes `bytes` to `target` whole or not at all: into `<target>.tmp` first, then renamed over the target. A failure leaves no `.tmp` behind.
pub fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("cannot write {}: {e}", target.display());
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = target.with_file_name(name);
    let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, target));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(e));
    }
    Ok(())
}

/// Deletes the .dvcf files under `root` last modified before `now - older_than` and returns how many went. Anything else is left alone.
/// An unreadable folder or a failed delete is one `warn` and the walk goes on; a missing `root` is simply 0.
pub fn sweep(root: &Path, older_than: Duration, now: SystemTime, warn: &dyn Fn(String)) -> usize {
    let Some(cutoff) = now.checked_sub(older_than) else { return 0 };
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            warn(format!("cannot read {}: {e}", root.display()));
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            removed += sweep(&path, older_than, now, warn);
        } else if kind.is_file() && path.extension().is_some_and(|x| x == "dvcf") {
            let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff);
            if old {
                match std::fs::remove_file(&path) {
                    Ok(()) => removed += 1,
                    Err(e) => warn(format!("cannot delete {}: {e}", path.display())),
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunk_recorder_plugin::testing;

    #[test]
    fn the_target_is_beside_the_json_or_under_the_output_folder() {
        let json = Path::new("cap").join("sys1").join("2026").join("10").join("29").join("9170-1761700000_851012500.0-call_3.json");
        assert_eq!(target(None, &json, "sys1/2026/10/29/9170-1761700000_851012500.0-call_3"), json.with_extension("dvcf"));
        assert!(target(None, &json, "x").to_string_lossy().ends_with("call_3.dvcf"), "the dot in the base name survives");
        let out = Path::new("out");
        assert_eq!(
            target(Some(out), &json, "sys1/2026/10/29/9170-1761700000_851012500.0-call_3"),
            out.join("sys1/2026/10/29/9170-1761700000_851012500.0-call_3.dvcf")
        );
    }

    #[test]
    fn a_write_is_whole_or_absent() {
        let dir = testing::temp_dir("output");
        let t = dir.join("a").join("b").join("c.dvcf");
        write_atomic(&t, b"hello").unwrap();
        assert_eq!(std::fs::read(&t).unwrap(), b"hello");
        assert!(!t.with_file_name("c.dvcf.tmp").exists());
        write_atomic(&t, b"again").unwrap();
        assert_eq!(std::fs::read(&t).unwrap(), b"again", "overwritten in place");
        // A target that cannot be written (its parent is a file): an error naming it, no .tmp left behind.
        let blocked = t.join("x.dvcf");
        let e = write_atomic(&blocked, b"x").unwrap_err();
        assert!(e.starts_with("cannot write ") && e.contains("x.dvcf"), "{e}");
    }

    #[test]
    fn the_sweep_deletes_only_old_dvcf_files() {
        let dir = testing::temp_dir("output");
        let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
        let mk = |rel: &str, when: Option<SystemTime>| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
            if let Some(t) = when {
                std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
            }
            p
        };
        let a = mk("sys1/2026/10/19/a.dvcf", Some(old));
        let b = mk("sys1/2026/10/29/b.dvcf", None);
        let wav = mk("sys1/2026/10/19/a.wav", Some(old));
        let tmp = mk("sys1/2026/10/19/c.dvcf.tmp", Some(old));
        let warned = std::cell::RefCell::new(Vec::new());
        let n = sweep(&dir, Duration::from_secs(7 * 86_400), SystemTime::now(), &|m| warned.borrow_mut().push(m));
        assert_eq!((n, warned.borrow().len()), (1, 0));
        assert!(!a.exists() && b.exists() && wav.exists() && tmp.exists());
        assert_eq!(sweep(&dir.join("nowhere"), Duration::from_secs(1), SystemTime::now(), &|_| panic!("no warning for a missing root")), 0);
    }
}
