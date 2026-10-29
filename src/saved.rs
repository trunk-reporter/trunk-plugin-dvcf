//! The saved queue: live calls still waiting when the plugin shut down are kept in `<data_dir>/queue.jsonl`, one JSON call per line, and picked up at the next start.

use std::path::Path;

use trunk_recorder_plugin::ConcludedCall;

/// The file inside the data folder that holds the saved calls.
pub const FILE: &str = "queue.jsonl";

/// Keeps `calls` for the next start, one per line, whole or not at all. An empty slice removes any earlier file instead.
pub fn save(data_dir: &Path, calls: &[ConcludedCall]) -> Result<(), String> {
    let target = data_dir.join(FILE);
    if calls.is_empty() {
        return match std::fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", target.display())),
        };
    }
    let mut text = String::new();
    for call in calls {
        text.push_str(&serde_json::to_string(call).map_err(|e| format!("cannot write {}: {e}", target.display()))?);
        text.push('\n');
    }
    crate::output::write_atomic(&target, text.as_bytes())
}

/// Reads the saved calls back, in the order they were saved, and removes the file so they are only taken once.
/// A line that does not parse is one `warn` (`queue.jsonl line <n>: <why>`) and is skipped; no file is simply no calls.
pub fn load(data_dir: &Path, warn: &dyn Fn(String)) -> Vec<ConcludedCall> {
    let target = data_dir.join(FILE);
    let text = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn(format!("cannot read {}: {e}", target.display()));
            return Vec::new();
        }
    };
    if let Err(e) = std::fs::remove_file(&target) {
        warn(format!("cannot remove {}: {e}", target.display()));
    }
    let mut calls = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ConcludedCall>(line) {
            Ok(call) => calls.push(call),
            Err(e) => warn(format!("{FILE} line {}: {e}", index + 1)),
        }
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunk_recorder_plugin::testing;

    #[test]
    fn saved_calls_come_back_in_order_and_the_file_is_gone() {
        let dir = testing::temp_dir("saved");
        let calls = vec![testing::call(&dir, "sys1", 100), testing::call(&dir, "sys1", 200), testing::call(&dir, "sys1", 300)];
        save(&dir, &calls).unwrap();
        assert!(dir.join(FILE).exists());
        let warnings = std::cell::RefCell::new(Vec::new());
        let back = load(&dir, &|w| warnings.borrow_mut().push(w));
        assert!(warnings.borrow().is_empty(), "{:?}", warnings.borrow());
        assert_eq!(back.len(), 3);
        for (a, b) in calls.iter().zip(&back) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.call.talkgroup, b.call.talkgroup);
        }
        assert!(!dir.join(FILE).exists());
        assert!(load(&dir, &|w| warnings.borrow_mut().push(w)).is_empty());
        assert!(warnings.borrow().is_empty());
    }

    #[test]
    fn an_empty_save_removes_the_file_and_a_bad_line_is_skipped_with_a_warning() {
        let dir = testing::temp_dir("saved");
        let good = testing::call(&dir, "sys1", 100);
        save(&dir, std::slice::from_ref(&good)).unwrap();
        assert!(dir.join(FILE).exists());
        save(&dir, &[]).unwrap();
        assert!(!dir.join(FILE).exists());
        save(&dir, &[]).unwrap();

        let line = serde_json::to_string(&good).unwrap();
        std::fs::write(dir.join(FILE), format!("{{\"path\": 1}}\n{line}\n")).unwrap();
        let warnings = std::cell::RefCell::new(Vec::new());
        let back = load(&dir, &|w| warnings.borrow_mut().push(w));
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].path, good.path);
        assert_eq!(back[0].call.talkgroup, good.call.talkgroup);
        let warnings = warnings.borrow();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("queue.jsonl line 1: "), "{}", warnings[0]);
    }
}
