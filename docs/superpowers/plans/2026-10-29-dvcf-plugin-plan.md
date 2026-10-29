# DVCF Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Trunk Recorder Pro plugin, `dvcf`, that writes every digital call as one self-contained SymbolStream v2 `.dvcf` file from the vocoder frames TRP captures beside the call, live and by catch-up.

**Architecture:** One Rust binary in five parts: the SDK's protocol loop (never blocks); a frames reader that turns a call's `.sdr` or `.frames.jsonl` into `VoiceFrame`s through TRP's own `trunk-core`; a pure DVCF builder (record + frames → bytes); a small priority work queue with one worker (live calls first, catch-up behind); and a catch-up pass that pages TRP's call store and queues what still has a frames file and no `.dvcf`. An upkeep thread runs the periodic pass, the retention sweep, metrics and status.

**Tech Stack:** Rust edition 2024, `rust-version = "1.99"`; `trunk-recorder-plugin` 0.9.0 and `trunk-core` as git dependencies on TRP PR #104's branch at one pinned revision; `serde`, `serde_json`, `schemars` 1. Nothing else at runtime.

**Spec:** `docs/superpowers/specs/2026-10-29-dvcf-plugin-design.md` (a copy of the approved design; the original lives in `D:\reporter`). The DVCF wire format is `DVCF_SPEC.md` in `https://github.com/trunk-reporter/tr-plugin-dvcf` (§3 binary layout, §6 codec registry, §9 file layout); its byte offsets are repeated in Task 3 so the implementer needs nothing else.

## Global Constraints

- Plugin id, crate name and executable name: `dvcf`. Display name `DVCF`. Description (Cargo.toml): `Writes every digital call as a self-contained .dvcf file (SymbolStream v2) from Trunk Recorder Pro's frame capture.` Repo `https://github.com/trunk-reporter/trunk-plugin-dvcf`, license `GPL-3.0-or-later` (the `LICENSE` file is the GPL-3.0 text).
- `edition = "2024"`, `rust-version = "1.99"`. `rustfmt.toml`: `max_width = 160`, `use_small_heuristics = "Max"`. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass at every commit.
- Dependencies, both at the same revision and moved together:
  `trunk-recorder-plugin = { git = "https://github.com/TrunkRecorder/trunk-recorder-pro", rev = "d9b1f16dd49d507e22d0835039144ac4e1e7e43b" }`
  `trunk-core = { git = "https://github.com/TrunkRecorder/trunk-recorder-pro", rev = "d9b1f16dd49d507e22d0835039144ac4e1e7e43b" }`
  plus `schemars = "1"`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`. No other runtime dependency.
- Never `println!` (stdout is the protocol). Logs go through `Host`.
- Every `call.concluded` on the live path gets exactly one `call.result`, except calls saved across a shutdown (none, as the spec says). Catch-up jobs send none.
- Settings keys, camelCase, exactly: `outputDir` (""), `retentionDays` (0), `catchUp` (true), `catchUpDays` (30); per system `enabled` (true).
- The file is SymbolStream v2, wire version byte 2, little-endian throughout, messages in the order CALL_START, CODEC_FRAME×N, CALL_METADATA, CALL_END, no HEARTBEAT.
- Frames are written in order, bit for bit as the frames file holds them, never reordered, dropped or repaired.
- Every `.dvcf` is written to `<name>.dvcf.tmp` and renamed over `<name>.dvcf`: whole or absent.
- Tests use the SDK's `testing` module (`testing::run`, `testing::capture`, `testing::hello`, `testing::call`, `testing::temp_dir`) and `trunk-core`'s own writers (`call_sdr`, `frames_jsonl`) for fixtures. No live recorder.

## Decisions the plan makes (where the spec left room)

- **Module layout** (file structure below): the `.sdr` reader is wrapped in `frames.rs` behind one function, `read(path, source) -> Result<Vec<VoiceFrame>, String>`, so a future "JSON lines for every mode" option in TRP changes only `frames.rs`.
- **Queue kinds** are `Live` (answered with `call.result`), `Saved` (a live call restored from `queue.jsonl`: worked in the live lane, answered with nothing) and `CatchUp` (behind both, answered with nothing). There is no queue-full rule: live calls are never dropped; the catch-up pass pauses when `2000` catch-up jobs wait and the next pass takes up the rest. Retries, for I/O errors only: after 1 s, 5 s and 15 s, then `failed, "gave up: <error>"`.
- **The start pass walks its whole window** (`catchUpDays`); only periodic passes stop at the first page where nothing was queued. A periodic pass after a paused one walks the whole window too. Otherwise a gap older than the newest page would never be filled.
- **Timestamps** come from `start_time_ms` when it is above 0, else `start_time × 1000` (older Trunk Recorder records have only seconds); `duration_ms` likewise from `call_length_ms` else `call_length × 1000`.
- **Codec type by the frame, then the mode:** an IMBE frame is type 0 whatever `voice_mode` says; an AMBE frame is 1 under `p25_phase2`, 2 under `dmr`, 6 under `nxdn48` / `nxdn96`, and 1 with one `warn` per call under any other mode. Nothing fails for its codec; only a frames file with no frames is `failed`.
- **`src_id`** of a frame or CALL_END is the `SrcEntry.src` as `u32`; a negative or over-range value (classic Trunk Recorder writes −1 for unknown) is 0.
- **Shutdown saves rather than fails** every live call still waiting *and* any live call in flight that has not finished by the deadline (its late finish sends nothing). Saved calls go to `<data_dir>/queue.jsonl`, one `ConcludedCall` JSON per line. At the next start they are pushed as `Saved` before the catch-up pass; one whose frames file is gone is dropped with an `info` line.
- **The catch-up job checks for the `.dvcf` again when it runs** (the live path may have written it since the page was read) and skips silently if it is there.
- **Status** is `error` only when `outputDir` is set and cannot be created at start or on new settings (the plugin runs on and answers `failed` per call); `warning` while the last live call had no frames file, or the call store answered an error to the last pass; else `ok` with the output folder ("beside the calls" or the folder).
- **Metrics** every 10 s from the upkeep thread: `queued`, `in_flight`, `retries`, `latency_ms`, `last_ok`, `last_error`, `last_error_text`, and `extra`: `written`, `bytes_written`, `failed`, `skipped` (an object: reason → count), `catch_up_queued`, `catch_up_written` (run totals).
- **Validation of settings**: `outputDir`, when set, must be an absolute path (the message names the field as the form shows it: `Output folder`). The other fields take any value.

## Review Focus

Input classes the spec implies but no task's tests would otherwise exercise, most likely to bite first:

1. A record with `start_time_ms` 0 (a call JSON from an older recorder, or a hand-made one): frame timestamps come from `start_time`, not 0. Pinned in Task 3 (`an_old_record_without_ms_fields_still_gets_timestamps`).
2. A `srcList` with `src` −1 or a position past the call's end: `src_id` 0 for the unknown talker, and the last entry whose `pos` is not after the frame. Pinned in Task 3 (`src_id_follows_the_talker_at_each_frame_and_unknown_is_zero`).
3. A `.frames.jsonl` for a call that is not NXDN (TRP writes one whenever an `.sdr` cannot be made, mixed codecs included): each frame keeps its own codec and param count, AMBE under `p25` is type 1 with a warning. Pinned in Task 3 (`ambe_under_an_unexpected_mode_is_type_1_with_a_warning`) and Task 2 (`a_jsonl_with_both_codecs_reads_each_as_its_own`).
4. A call whose JSON sits outside the capture folder (the RAM spool): the frames file is found beside the JSON. Pinned in Task 2 (`the_frames_file_is_found_beside_the_json_before_the_capture_folder`).
5. The same call live and in a catch-up page at once, or concluded twice: one file, one `call.result`, no second job. Pinned in Task 5 (`a_duplicate_path_is_queued_once_and_a_live_push_makes_it_live`) and Task 9 (`a_call_concluded_twice_gets_one_answer_each`).

---

## File structure

```
Cargo.toml                 crate `dvcf`, deps above, release profile strip+lto
rustfmt.toml, .gitignore, LICENSE (GPL-3.0), README.md, CHANGELOG.md
.github/workflows/ci.yml, release.yml, .github/check_manifest.py   (copied from D:\trunk-plugin-transcribe, verbatim)
scripts/validate_dvcf_fixture.py   tr-plugin-dvcf's validator, verbatim (run by one test when Python is present)
src/main.rs        trunk_recorder_plugin::run::<plugin::Dvcf>()
src/lib.rs         pub mod {config, frames, dvcf, output, queue, work, catchup, saved, plugin}; #[cfg(test)] pub mod fixtures
src/config.rs      Config, SystemConfig, defaults, validation
src/fixtures.rs    (test only) synthetic VoiceFrames, calls with an .sdr / .frames.jsonl beside them
src/frames.rs      find a call's frames file; read it to Vec<VoiceFrame> (.sdr through trunk-core, .frames.jsonl parsed)
src/dvcf.rs        the builder: record + frames → the file's bytes; the test-only reader
src/output.rs      where a call's .dvcf goes; the atomic write; the retention sweep
src/queue.rs       the work queue: live and saved first, catch-up behind, retries, the shutdown drain that returns what to save
src/work.rs        Ctx (settings, systems, counters); the skip rules; one job from frames file to .dvcf
src/catchup.rs     the catch-up pass over calls.query pages
src/saved.rs       queue.jsonl: save at shutdown, load at start
src/plugin.rs      Dvcf: Plugin — start, call_concluded, calls, settings, shutdown, the upkeep thread
```

---

### Task 1: Repository scaffold, settings and manifest

**Files:**
- Create: `Cargo.toml`, `rustfmt.toml`, `.gitignore`, `LICENSE`, `README.md`, `CHANGELOG.md`, `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `.github/check_manifest.py`, `scripts/validate_dvcf_fixture.py`, `src/main.rs`, `src/lib.rs`, `src/config.rs`, `src/plugin.rs` (a stub that only starts)
- Test: `src/config.rs`, `src/plugin.rs`

**Interfaces:**
- Produces: `config::Config { output_dir: String, retention_days: u32, catch_up: bool, catch_up_days: u32 }` (`#[serde(rename_all = "camelCase", default)]`, `Clone + Debug + PartialEq + Serialize + Deserialize + JsonSchema`), `impl Default` with the spec's defaults (`""`, 0, true, 30).
- Produces: `config::SystemConfig { enabled: bool }` (default true).
- Produces: `Config::validate(&self) -> Result<(), String>`: `output_dir` empty or an absolute path (`Path::is_absolute`), else `Err("Output folder: must be an absolute path")`.
- Produces: `Config::output_dir(&self) -> Option<PathBuf>`: `None` when empty.
- Produces: `plugin::Dvcf` with `manifest()` = `Manifest { name: "DVCF", subscribe: vec![topic::CALL_CONCLUDED], audio_formats: vec![], live_settings: true, ..manifest!() }`.

- [ ] **Step 1: Write the scaffold**

`Cargo.toml` as the Global Constraints say (`[package] name = "dvcf"`, `version = "0.1.0"`, `publish = false`, `[profile.release] strip = true, lto = true`). Copy `rustfmt.toml`, `.gitignore`, `.github/workflows/ci.yml`, `.github/workflows/release.yml` and `.github/check_manifest.py` from `D:\trunk-plugin-transcribe` unchanged except the `.gitignore` comment that names the executable (`dvcf --describe`). `LICENSE`: the GPL-3.0 text (`curl -L https://www.gnu.org/licenses/gpl-3.0.txt`). `scripts/validate_dvcf_fixture.py`: `gh api repos/trunk-reporter/tr-plugin-dvcf/contents/scripts/validate_dvcf_fixture.py --jq .content | base64 -d`, with a first-line comment `# From trunk-reporter/tr-plugin-dvcf, unchanged: the C++ plugin's own check of a .dvcf file.` `README.md`: title, one paragraph, a "Settings" heading saying "see the design document" for now. `CHANGELOG.md`: `## [0.1.0]` with one line.

`src/config.rs`: doc comments on every field in the form's style (first line the label, a blank line, then help):
- `output_dir`: `Output folder` / `Where the .dvcf files go. Empty: beside each call in the capture folder. Otherwise this folder, with the calls' own system/date layout under it.`
- `retention_days`: `Keep for (days)` / `Delete .dvcf files older than this. 0 keeps everything. Only .dvcf files are touched.`
- `catch_up`: `Catch up` / `Convert past calls that still have a frames file and no .dvcf: at start, then every 10 minutes.`
- `catch_up_days`: `Catch up (days)` / `How far back the catch-up looks. 0: as far as the call store goes.`
- `SystemConfig::enabled`: `Write this system` / `Turn off to leave this system's calls out.`

`src/plugin.rs` for now: `pub struct Dvcf { host: Host }`, `start` validates `setup.config` and returns `Ok`, nothing else. `src/lib.rs`: `pub mod config; pub mod plugin;` (the rest are added by their tasks).

- [ ] **Step 2: Write the failing tests** in `src/config.rs` and `src/plugin.rs`

```rust
// src/config.rs
#[test]
fn defaults_are_the_specs() {
    let c = Config::default();
    assert_eq!((c.output_dir.as_str(), c.retention_days, c.catch_up, c.catch_up_days), ("", 0, true, 30));
    assert!(SystemConfig::default().enabled);
    assert_eq!(c.output_dir(), None);
}

#[test]
fn a_relative_output_folder_is_refused_by_name() {
    let mut c = Config::default();
    assert!(c.validate().is_ok());
    c.output_dir = "dvcf".into();
    assert_eq!(c.validate().unwrap_err(), "Output folder: must be an absolute path");
    c.output_dir = if cfg!(windows) { r"D:\dvcf".into() } else { "/srv/dvcf".into() };
    assert!(c.validate().is_ok());
    assert!(c.output_dir().is_some());
}

#[test]
fn the_schema_names_the_fields_in_camel_case() {
    let s = trunk_recorder_plugin::schema::schema_for::<Config>();
    let keys: Vec<&String> = s["properties"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["outputDir", "retentionDays", "catchUp", "catchUpDays"]);
    assert_eq!(s["properties"]["catchUpDays"]["default"], serde_json::json!(30));
}
```

```rust
// src/plugin.rs
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
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test`
Expected: compile errors for the missing types. (The first build fetches and compiles `trunk-core`; several minutes. If `trunk-core` fails to resolve as a git dependency, the error is the task's finding: report it rather than work around it.)

- [ ] **Step 4: Implement `config.rs` and the stub plugin**

- [ ] **Step 5: Run the tests, clippy and fmt**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check && cargo run --quiet -- --describe > manifest.json && python .github/check_manifest.py manifest.json`
Expected: all pass; the manifest check prints no error.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "Scaffold: the dvcf plugin's settings, manifest, licence and release workflow"
```

---

### Task 2: The frames file — find it, read it

**Files:**
- Create: `src/fixtures.rs` (test only), `src/frames.rs`
- Modify: `src/lib.rs`
- Test: `src/frames.rs`

**Interfaces:**
- Consumes (trunk-core): `trunk_core::trunk::frames::{Codec, VoiceFrame, hex_bits, frames_jsonl}`, `trunk_core::trunk::sdr::{sdr_frames, call_sdr, SdrMode, SdrInfo}`, `trunk_core::mbe::Kind`.
- Produces: `pub enum Source { Sdr, Jsonl }` (`Copy + Debug + PartialEq`) with `Source::name(self) -> &'static str` (`"sdr"` / `"frames.jsonl"`, the value `trunk_recorder_pro.frames_file` carries) and `Source::extension(self) -> &'static str` (the same strings).
- Produces: `pub fn find(json: &Path, capture_dir: &Path, path: &str) -> Option<(PathBuf, Source)>`: tries, in order, `json` with its extension swapped for `sdr`, then `frames.jsonl`; then `<capture_dir>/<path>.sdr`, then `.frames.jsonl`; the first that exists. ("Swapped" means `json.with_extension("sdr")` and `json.with_extension("frames.jsonl")`; a call key with a dot in its base name keeps it, as `Path::with_extension` replaces only the last extension.)
- Produces: `pub fn read(file: &Path, source: Source) -> Result<Vec<VoiceFrame>, String>`: `Sdr` → `sdr_frames(&bytes)`; `Jsonl` → one `VoiceFrame` per non-empty line from `{"pos","codec","bits","e0","errs","erased","out"}` (`codec` `"imbe"` → 88 bits, `"ambe"` → 49, through `hex_bits`; `out` → `Kind` by name; `tone: None`); a line that does not parse is `Err("line <n>: <why>")`; an unreadable file is `Err("cannot read <file>: <io error>")`.
- Produces (fixtures, `#[cfg(test)]`): `pub fn frames(codec: Codec, n: usize) -> Vec<VoiceFrame>` — frame k has bits `(k * 7 + i * 3) % 5 == 0` for bit i, `e0 = k % 4` (IMBE only), `errs = k % 6`, `erased = k == 3 && codec == Codec::Imbe`, `kind` = `Voice` except k 1 `Repeat`, k 2 `Muted`, k 4 `Tone` (AMBE only; IMBE k 4 `Voice`), `tone: None`; `pub fn sdr_call(dir: &Path, mode: SdrMode, tg: u32, n: usize) -> (ConcludedCall, Vec<VoiceFrame>)` — `testing::call(dir, "sys1", tg)` with `voice_mode` set to `"p25"` / `"p25_phase2"` / `"dmr"`, `call_num = tg`, `start_time_ms = start_time * 1000 + 250`, `call_length_ms = n as i64 * 20`, two `srcList` entries (`src` 1234 at pos 0.0, `src` 5678 at pos `(n / 2) as f64 * 0.02`), the `.sdr` from `call_sdr(mode, &frames, &SdrInfo { start_us, duration_ms, hz, tg, ..Default::default() })` written beside `files.json`, and the record's `.json` rewritten with `to_json`; `pub fn jsonl_call(dir, tg, frames: &[VoiceFrame]) -> ConcludedCall` likewise with `voice_mode = "nxdn48"` and `frames_jsonl` written to `<base>.frames.jsonl`.

- [ ] **Step 1: Write the failing tests** in `src/frames.rs`

```rust
#[test]
fn an_sdr_of_each_mode_reads_back_the_frames_it_was_made_from() {
    let dir = testing::temp_dir("frames");
    for (mode, codec) in [(SdrMode::P25Fdma, Codec::Imbe), (SdrMode::P25Tdma, Codec::Ambe), (SdrMode::Dmr, Codec::Ambe)] {
        let (call, frames) = fixtures::sdr_call(&dir, mode, 10, 27);
        let (file, source) = find(&call.files.json, &dir, &call.path).expect("found beside the json");
        assert_eq!(source, Source::Sdr);
        let read = read(&file, Source::Sdr).unwrap();
        assert_eq!(read.len(), 27, "{mode:?}");
        for (k, (a, b)) in read.iter().zip(&frames).enumerate() {
            assert_eq!((a.codec, &a.bits, a.kind), (codec, &b.bits, b.kind), "{mode:?} frame {k}");
            if codec == Codec::Imbe { assert_eq!((a.erased, a.e0), (b.erased, b.e0), "frame {k}"); }
            if !b.erased { assert_eq!(a.errs, b.errs, "{mode:?} frame {k}"); }
        }
    }
}

#[test]
fn a_jsonl_with_both_codecs_reads_each_as_its_own() {
    let dir = testing::temp_dir("frames");
    let mut frames = fixtures::frames(Codec::Ambe, 5);
    frames.extend(fixtures::frames(Codec::Imbe, 3));
    let call = fixtures::jsonl_call(&dir, 11, &frames);
    let (file, source) = find(&call.files.json, &dir, &call.path).unwrap();
    assert_eq!(source, Source::Jsonl);
    assert_eq!(read(&file, Source::Jsonl).unwrap(), frames);
}

#[test]
fn the_frames_file_is_found_beside_the_json_before_the_capture_folder() {
    let dir = testing::temp_dir("frames");
    let spool = testing::temp_dir("spool");
    let (call, _) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 12, 9);
    // The same call held in a spool elsewhere: its JSON and frames are there, not under the capture folder.
    let json = spool.join("c.json");
    std::fs::copy(&call.files.json, &json).unwrap();
    std::fs::copy(call.files.json.with_extension("sdr"), spool.join("c.sdr")).unwrap();
    std::fs::remove_file(call.files.json.with_extension("sdr")).unwrap();
    assert_eq!(find(&json, &dir, &call.path), Some((spool.join("c.sdr"), Source::Sdr)));
    // Gone from both places: none.
    std::fs::remove_file(spool.join("c.sdr")).unwrap();
    assert_eq!(find(&json, &dir, &call.path), None);
    // Under the capture folder by the call's key, with the JSON elsewhere: found there.
    std::fs::write(dir.join(format!("{}.frames.jsonl", call.path)), "").unwrap();
    assert_eq!(find(&json, &dir, &call.path), Some((dir.join(format!("{}.frames.jsonl", call.path)), Source::Jsonl)));
}

#[test]
fn bad_files_are_errors_that_say_where() {
    let dir = testing::temp_dir("frames");
    assert!(read(&dir.join("none.sdr"), Source::Sdr).unwrap_err().starts_with("cannot read "));
    let f = dir.join("x.frames.jsonl");
    std::fs::write(&f, "{\"pos\":0.0,\"codec\":\"imbe\",\"bits\":\"zz\",\"e0\":0,\"errs\":0,\"erased\":false,\"out\":\"voice\"}\n").unwrap();
    assert!(read(&f, Source::Jsonl).unwrap_err().starts_with("line 1: "));
    std::fs::write(&f, "").unwrap();
    assert_eq!(read(&f, Source::Jsonl).unwrap(), vec![]);
    std::fs::write(&f, "not sdr at all").unwrap();
    assert_eq!(read(&f, Source::Sdr).unwrap(), vec![], "trunk-core reads no records from it");
}
```

- [ ] **Step 2: Run `cargo test frames` to see them fail.**
- [ ] **Step 3: Implement `src/fixtures.rs` and `src/frames.rs`**; add `pub mod frames;` and `#[cfg(test)] pub mod fixtures;` to `lib.rs`. For the `.sdr` fixtures keep `errs` under 32 and IMBE `e0` under 32 (the tail byte holds five bits), as the fixture rule above does.
- [ ] **Step 4: `cargo test frames` → pass; clippy and fmt clean.**
- [ ] **Step 5: Commit** — `git commit -am "Frames: a call's .sdr or .frames.jsonl found beside it and read back through trunk-core"`

---

### Task 3: The DVCF builder

**Files:**
- Create: `src/dvcf.rs`
- Modify: `src/lib.rs`
- Test: `src/dvcf.rs`

**Interfaces:**
- Consumes: `frames::Source`; `trunk_core::p25::voice::imbe_bits_to_params(&[u8; 88]) -> [u32; 8]`; `trunk_core::trunk::frames::{Codec, VoiceFrame}`; `trunk_core::mbe::Kind`; `trunk_recorder_plugin::CallRecord` (`to_json`, `spike_count`).
- Produces: `pub struct Provenance<'a> { pub host_version: &'a str, pub plugin_version: &'a str, pub source: Source }`.
- Produces: `pub struct Built { pub bytes: Vec<u8>, pub frames: usize, pub warnings: Vec<String> }`.
- Produces: `pub fn build(record: &CallRecord, frames: &[VoiceFrame], p: Provenance) -> Result<Built, String>`: `Err("no frames")` for an empty slice; otherwise the four message kinds in order, exactly:
  - every message: `b"SY"`, `0x02`, `msg_type`, `payload_len: u32 LE`, payload.
  - CALL_START (0x02): `talkgroup: u32`, `freq: u64`, `start_us: u64`, `call_id: u32` (`record.call_num as u32`), `name_len: u8`, `short_name` bytes (cut to 255 bytes on a char boundary).
  - CODEC_FRAME (0x01) for frame k: `talkgroup: u32`, `src_id: u32`, `call_id: u32`, `start_us + k × 20_000: u64`, `codec_type: u8`, `param_count: u8` (8 / 4), `errs: u8` (`min(errs, 255)`, 0 when `erased`), `flags: u8` (bit 0 when `erased` or `kind` is `Repeat` / `Muted` / `Erasure`), then `param_count × u32 LE`: IMBE `imbe_bits_to_params`, AMBE the 49 bits split 12 / 12 / 11 / 14, MSB first within each word.
  - CALL_METADATA (0x05): the JSON object of Step 3's `metadata(record, p)`.
  - CALL_END (0x03): `talkgroup: u32`, `call_id: u32`, `src_id: u32` (the last `srcList` entry, else 0), `freq: u64`, `duration_ms: u32`, `error_count: u32` (the sum of the written `errs` values, saturating), `encrypted: u8`, `name_len: u8`, `short_name`.
  - `start_us` = `start_time_ms × 1000` when `start_time_ms > 0`, else `start_time × 1_000_000`; `duration_ms` = `call_length_ms` when `> 0`, else `(call_length × 1000).round()`.
  - `src_id` for frame k: among `srcList` entries with `pos ≤ k × 0.02` the one with the greatest `pos` (the last such in list order on a tie); its `src` as `u32` via `u32::try_from(src).unwrap_or(0)`; 0 when none.
  - `codec_type`: IMBE 0; AMBE by `voice_mode`: `p25_phase2` 1, `dmr` 2, `nxdn48` / `nxdn96` 6, anything else 1 and one warning `AMBE frames under voice mode "<mode>": written as P25 Phase 2 (type 1)` in `Built::warnings` (once per call).
- Produces: `pub fn metadata(record: &CallRecord, p: Provenance) -> serde_json::Value`: an object with, in this order: `tg_tag` (`talkgroup_group_tag`), `tg_alpha_tag` (`talkgroup_tag`), `tg_group` (`talkgroup_group`), `tg_description` (`talkgroup_description`), each only when non-empty; `signal`, `noise` (`Option<f64>`: JSON null when None), `freq_error`, `spike_count` (`record.spike_count()`), `emergency`, `priority`, `phase2_tdma`, `tdma_slot` always; `patched_tgs` (`patched_talkgroups`) when non-empty; `src_list` when non-empty, each `{ "src", "time", "pos", "emergency": 0 | 1, "signal_system", "tag" }`; `trunk_recorder_call`: `serde_json::from_str(&record.to_json())`; `trunk_recorder_pro`: `{ "version": host_version, "plugin": "dvcf <plugin_version>", "frames_file": source.name() }`.
- Produces (test only, `#[cfg(test)] pub mod read`): `pub enum Msg { Start { tg: u32, freq: u64, ts: u64, call_id: u32, name: String }, Frame { tg: u32, src: u32, call_id: u32, ts: u64, codec: u8, errs: u8, flags: u8, params: Vec<u32> }, Metadata(Value), End { tg: u32, call_id: u32, src: u32, freq: u64, dur_ms: u32, errs: u32, enc: bool, name: String } }` and `pub fn messages(bytes: &[u8]) -> Vec<Msg>` (panics on a bad header, a wrong version, or a truncated payload).

- [ ] **Step 1: Write the failing tests** in `src/dvcf.rs` (fixtures from Task 2; `p()` is `Provenance { host_version: "0.3.0", plugin_version: "0.1.0", source: Source::Sdr }`)

```rust
#[test]
fn an_imbe_call_is_start_frames_metadata_end() {
    let dir = testing::temp_dir("dvcf");
    let (call, frames) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 20, 12);
    let r = &call.call;
    let built = build(r, &frames, p()).unwrap();
    assert_eq!((built.frames, built.warnings.len()), (12, 0));
    let m = read::messages(&built.bytes);
    assert_eq!(m.len(), 15);
    let start_us = r.start_time_ms as u64 * 1000;
    assert_eq!(m[0], Msg::Start { tg: 20, freq: 851_012_500, ts: start_us, call_id: 20, name: "sys1".into() });
    let Msg::Frame { tg, src, call_id, ts, codec, errs, flags, ref params } = m[1] else { panic!("{:?}", m[1]) };
    assert_eq!((tg, src, call_id, ts, codec, errs, flags), (20, 1234, 20, start_us, 0, 0, 0));
    assert_eq!(params, &imbe_bits_to_params(frames[0].bits.as_slice().try_into().unwrap()).to_vec());
    let Msg::Frame { ts, src, .. } = m[7] else { panic!() };
    assert_eq!((ts, src), (start_us + 6 * 20_000, 5678), "the second talker from frame 6 (pos 0.12)");
    // Frame 1 is a repeat, 2 muted, 3 erased: silence flag; errs 0 for the erased one.
    let flags_errs: Vec<(u8, u8)> = m[1..13].iter().map(|f| match f { Msg::Frame { flags, errs, .. } => (*flags, *errs), _ => panic!() }).collect();
    assert_eq!(&flags_errs[..5], &[(0, 0), (1, 1), (1, 2), (1, 0), (0, 4)]);
    assert!(matches!(m[13], Msg::Metadata(_)));
    let sum: u32 = flags_errs.iter().map(|(_, e)| *e as u32).sum();
    assert_eq!(m[14], Msg::End { tg: 20, call_id: 20, src: 5678, freq: 851_012_500, dur_ms: 240, errs: sum, enc: false, name: "sys1".into() });
    // CALL_START is 8 + 29 bytes ("sys1"), then every IMBE frame is 8 + 24 + 32 = 64 bytes.
    assert_eq!(&built.bytes[37..41], b"SY\x02\x01");
    assert_eq!(&built.bytes[101..105], b"SY\x02\x01");
}

#[test]
fn ambe_calls_take_their_type_from_the_mode_and_split_the_words() {
    let dir = testing::temp_dir("dvcf");
    for (mode, voice_mode, want) in [(SdrMode::P25Tdma, "p25_phase2", 1u8), (SdrMode::Dmr, "dmr", 2)] {
        let (mut call, frames) = fixtures::sdr_call(&dir, mode, 21, 6);
        call.call.voice_mode = voice_mode.into();
        let m = read::messages(&build(&call.call, &frames, p()).unwrap().bytes);
        let Msg::Frame { codec, ref params, flags, .. } = m[5] else { panic!() };
        assert_eq!((codec, params.len(), flags), (want, 4, 0), "{voice_mode}: frame 4 is a tone, flag clear");
        let b = &frames[4].bits;
        let word = |r: std::ops::Range<usize>| b[r].iter().fold(0u32, |v, &x| v << 1 | x as u32);
        assert_eq!(params, &vec![word(0..12), word(12..24), word(24..35), word(35..49)]);
    }
    let frames = fixtures::frames(Codec::Ambe, 4);
    let call = fixtures::jsonl_call(&dir, 22, &frames);
    let m = read::messages(&build(&call.call, &frames, Provenance { source: Source::Jsonl, ..p() }).unwrap().bytes);
    assert!(matches!(m[1], Msg::Frame { codec: 6, .. }), "nxdn48");
}

#[test]
fn ambe_under_an_unexpected_mode_is_type_1_with_a_warning() {
    let dir = testing::temp_dir("dvcf");
    let mut frames = fixtures::frames(Codec::Ambe, 3);
    frames.extend(fixtures::frames(Codec::Imbe, 2));
    let mut call = fixtures::jsonl_call(&dir, 23, &frames);
    call.call.voice_mode = "p25".into();
    let built = build(&call.call, &frames, p()).unwrap();
    assert_eq!(built.warnings, ["AMBE frames under voice mode \"p25\": written as P25 Phase 2 (type 1)"]);
    let codecs: Vec<(u8, usize)> = read::messages(&built.bytes).iter().filter_map(|m| match m { Msg::Frame { codec, params, .. } => Some((*codec, params.len())), _ => None }).collect();
    assert_eq!(codecs, [(1, 4), (1, 4), (1, 4), (0, 8), (0, 8)]);
}

#[test]
fn metadata_maps_the_talkgroup_columns_and_carries_the_whole_record() {
    let mut r = CallRecord { short_name: "sys1".into(), talkgroup: 9170, call_num: 7, freq: 855_737_500, ..Default::default() };
    r.talkgroup_tag = "BUTCO FD".into();
    r.talkgroup_group_tag = "Fire Dispatch".into();
    r.talkgroup_group = "Fire".into();
    r.signal = Some(-42.5);
    r.freq_error = 12;
    r.priority = 3;
    r.patched_talkgroups = vec![9171];
    r.src_list = vec![SrcEntry { src: 1234567, time: 1711234567, pos: 0.0, emergency: true, tag: "Engine 5".into(), ..Default::default() }];
    r.freq_list = vec![FreqEntry { freq: 855_737_500, spike_count: 2, ..Default::default() }];
    let m = metadata(&r, p());
    let keys: Vec<&String> = m.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["tg_tag", "tg_alpha_tag", "tg_group", "signal", "noise", "freq_error", "spike_count", "emergency", "priority", "phase2_tdma", "tdma_slot", "patched_tgs", "src_list", "trunk_recorder_call", "trunk_recorder_pro"]);
    assert_eq!((&m["tg_tag"], &m["tg_alpha_tag"], &m["tg_group"]), (&json!("Fire Dispatch"), &json!("BUTCO FD"), &json!("Fire")));
    assert_eq!((&m["signal"], &m["noise"], &m["freq_error"], &m["spike_count"], &m["priority"]), (&json!(-42.5), &Value::Null, &json!(12), &json!(2), &json!(3)));
    assert_eq!(m["src_list"], json!([{ "src": 1234567, "time": 1711234567, "pos": 0.0, "emergency": 1, "signal_system": "", "tag": "Engine 5" }]));
    assert_eq!(m["trunk_recorder_call"], serde_json::from_str::<Value>(&r.to_json()).unwrap());
    assert_eq!(m["trunk_recorder_pro"], json!({ "version": "0.3.0", "plugin": "dvcf 0.1.0", "frames_file": "sdr" }));
    // Nothing set: the strings and lists are left out, the levels are null.
    let bare = metadata(&CallRecord::default(), p());
    assert!(bare.get("tg_tag").is_none() && bare.get("patched_tgs").is_none() && bare.get("src_list").is_none());
    assert_eq!(bare["signal"], Value::Null);
}

#[test]
fn an_old_record_without_ms_fields_still_gets_timestamps() {
    let frames = fixtures::frames(Codec::Imbe, 2);
    let r = CallRecord { start_time: 1_700_000_000, call_length: 1.5, talkgroup: 1, ..Default::default() };
    let m = read::messages(&build(&r, &frames, p()).unwrap().bytes);
    assert!(matches!(m[0], Msg::Start { ts: 1_700_000_000_000_000, .. }));
    assert!(matches!(m[2], Msg::Frame { ts: 1_700_000_000_020_000, .. }));
    assert!(matches!(m[4], Msg::End { dur_ms: 1500, .. }));
}

#[test]
fn src_id_follows_the_talker_at_each_frame_and_unknown_is_zero() {
    let frames = fixtures::frames(Codec::Imbe, 5);
    let mut r = CallRecord { start_time_ms: 1_000, ..Default::default() };
    r.src_list = vec![
        SrcEntry { src: -1, pos: 0.0, ..Default::default() },
        SrcEntry { src: 7, pos: 0.04, ..Default::default() },
        SrcEntry { src: 9, pos: 0.04, ..Default::default() },
        SrcEntry { src: 11, pos: 9.0, ..Default::default() },
    ];
    let srcs: Vec<u32> = read::messages(&build(&r, &frames, p()).unwrap().bytes).iter().filter_map(|m| match m { Msg::Frame { src, .. } => Some(*src), _ => None }).collect();
    assert_eq!(srcs, [0, 0, 9, 9, 9], "-1 is unknown; a tie takes the later entry; a talker past the end never comes");
    assert!(matches!(read::messages(&build(&r, &frames, p()).unwrap().bytes).last(), Some(Msg::End { src: 11, .. })));
    assert_eq!(build(&r, &[], p()).unwrap_err(), "no frames");
}

#[test]
fn big_values_are_capped_not_wrapped() {
    let mut frames = fixtures::frames(Codec::Imbe, 2);
    frames[0].errs = 300;
    frames[1].errs = u32::MAX;
    let r = CallRecord { call_num: u64::from(u32::MAX) + 5, short_name: "s".repeat(300), ..Default::default() };
    let m = read::messages(&build(&r, &frames, p()).unwrap().bytes);
    assert!(matches!(m[0], Msg::Start { call_id: 4, ref name } if name.len() == 255));
    let errs: Vec<u8> = m.iter().filter_map(|x| match x { Msg::Frame { errs, .. } => Some(*errs), _ => None }).collect();
    assert_eq!(errs, [255, 255]);
    assert!(matches!(m[4], Msg::End { errs: 510, .. }));
}

#[test]
fn the_c_plugins_validator_passes_the_file() {
    let python = ["python3", "python"].into_iter().find(|p| std::process::Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success()));
    let Some(python) = python else { eprintln!("no python: skipped"); return };
    let dir = testing::temp_dir("dvcf");
    let (call, frames) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 24, 9);
    let f = dir.join("x.dvcf");
    std::fs::write(&f, build(&call.call, &frames, p()).unwrap().bytes).unwrap();
    let out = std::process::Command::new(python).args(["-I", "scripts/validate_dvcf_fixture.py"]).arg(&f).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok: 12 messages");
}
```

- [ ] **Step 2: Run `cargo test dvcf` to see them fail.**
- [ ] **Step 3: Implement `src/dvcf.rs`**: a `Writer` over a `Vec<u8>` with `message(&mut self, kind: u8, payload: &[u8])` and small `put_u32` / `put_u64` helpers; `build` composes the four kinds; `metadata` builds a `serde_json::Map` in the stated order. The test reader under `#[cfg(test)]`.
- [ ] **Step 4: `cargo test dvcf` → pass; clippy and fmt clean.**
- [ ] **Step 5: Commit** — `git commit -am "The DVCF builder: a call's record and frames as SymbolStream v2 bytes, the whole record in CALL_METADATA"`

---

### Task 4: Output — where the file goes, the atomic write, the retention sweep

**Files:**
- Create: `src/output.rs`
- Modify: `src/lib.rs`
- Test: `src/output.rs`

**Interfaces:**
- Produces: `pub fn target(output_dir: Option<&Path>, json: &Path, path: &str) -> PathBuf`: `None` → `json.with_extension("dvcf")`; `Some(dir)` → `dir.join(format!("{path}.dvcf"))` (the call's key keeps its `system/Y/M/D/base` layout).
- Produces: `pub fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), String>`: creates the parent folders, writes `<target>.tmp` (`target` with `.tmp` appended to its file name, so `x.dvcf.tmp`), renames it over `target`; on any error removes the `.tmp` and returns `"cannot write <target>: <io error>"`.
- Produces: `pub fn sweep(root: &Path, older_than: Duration, now: SystemTime, warn: &dyn Fn(String)) -> usize`: walks `root` recursively, deletes regular files whose name ends in `.dvcf` and whose modification time is before `now - older_than`, returns how many; anything else is left (other extensions, `.dvcf.tmp`, folders); an unreadable folder or a failed delete is one `warn` each and the walk goes on; a missing `root` is 0 with no warning.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_target_is_beside_the_json_or_under_the_output_folder() {
    let json = Path::new("cap").join("sys1").join("2026").join("10").join("29").join("9170-1761700000_851012500.0-call_3.json");
    assert_eq!(target(None, &json, "sys1/2026/10/29/9170-1761700000_851012500.0-call_3"), json.with_extension("dvcf"));
    assert!(target(None, &json, "x").to_string_lossy().ends_with("call_3.dvcf"), "the dot in the base name survives");
    let out = Path::new("out");
    assert_eq!(target(Some(out), &json, "sys1/2026/10/29/9170-1761700000_851012500.0-call_3"), out.join("sys1/2026/10/29/9170-1761700000_851012500.0-call_3.dvcf"));
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
        if let Some(t) = when { std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap(); }
        p
    };
    let a = mk("sys1/2026/10/19/a.dvcf", Some(old));
    let b = mk("sys1/2026/10/29/b.dvcf", None);
    let wav = mk("sys1/2026/10/19/a.wav", Some(old));
    let tmp = mk("sys1/2026/10/19/c.dvcf.tmp", Some(old));
    let mut warned = Vec::new();
    let n = sweep(&dir, Duration::from_secs(7 * 86_400), SystemTime::now(), &|m| warned.push(m));
    assert_eq!((n, warned.len()), (1, 0));
    assert!(!a.exists() && b.exists() && wav.exists() && tmp.exists());
    assert_eq!(sweep(&dir.join("nowhere"), Duration::from_secs(1), SystemTime::now(), &|_| panic!("no warning for a missing root")), 0);
}
```

- [ ] **Step 2: Run `cargo test output` to see them fail.**
- [ ] **Step 3: Implement `src/output.rs`.** The walk is `std::fs::read_dir` recursion (no crate); `set_modified` exists in std since 1.75.
- [ ] **Step 4: `cargo test output` → pass; clippy and fmt clean.**
- [ ] **Step 5: Commit** — `git commit -am "Output: the file beside the call or under the output folder, written whole, swept by age"`

---

### Task 5: The work queue

**Files:**
- Create: `src/queue.rs` (start from `D:\trunk-plugin-transcribe\src\queue.rs`, by the same author: keep its `Mutex<Inner> + Condvar` shape, its worker loop, `catch_unwind`, and its tests' style; change what this task says)
- Modify: `src/lib.rs`
- Test: `src/queue.rs`

**Interfaces:**
- Produces: `pub enum Kind { Live, Saved, CatchUp }` (`Clone + Copy + Debug + PartialEq`). `Live` and `Saved` share the front lane (oldest first); `CatchUp` waits behind them. Only `Live` sends `call.result`.
- Produces: `pub struct Job { pub call: ConcludedCall, pub kind: Kind, pub enqueued: Instant, pub tries: u32 }`.
- Produces: `pub enum Done { Ok { took: Duration, bytes: u64 }, Skipped(String), Failed(String), Retry(String) }`.
- Produces: `pub struct QueueOptions { pub retry_after: Vec<Duration> }` (the plugin passes 1 s, 5 s, 15 s). One worker thread, named `dvcf-worker`.
- Produces: `pub struct Queue` with:
  - `Queue::start(host: Host, opts: QueueOptions, work: impl Fn(&Job) -> Done + Send + Sync + 'static) -> Queue`.
  - `push(&self, call: ConcludedCall, kind: Kind) -> bool`: false (and nothing happens) when that `path` is queued or in flight, except that a `Live` push for a path queued as `Saved` or `CatchUp` makes that job `Live` (it then gets the `call.result`) and returns true; after `shutdown` began, a pushed `Live` job is answered at once with `failed, "shutting down"` and others are dropped.
  - the worker takes the front lane first (oldest first), then retries that are due, then `CatchUp` jobs. `Done::Retry` with `tries < retry_after.len()` waits `retry_after[tries]` and logs `warn` `retry <n>/<total> for <path>: <message>`; past that it ends `Failed("gave up: <message>")`. For `Live` jobs every end sends `call_result_took` (`Ok` with `took`), `call_result` (`Skipped`, `Failed`).
  - `snapshot(&self) -> Snapshot { queued, front, catch_up, retrying, in_flight: usize, retries, ok, failed, catch_up_ok, bytes: u64, latency_ms: Option<f64> (EWMA 0.2 of Ok took), last_ok_s, last_error_s: Option<u64>, last_error: String }` (`Clone + Debug + Default`); `catch_up_ok` counts `Ok` ends of `CatchUp` jobs, `bytes` sums `Done::Ok.bytes`.
  - `catch_up_waiting(&self) -> usize`, `len(&self) -> usize` (waiting + in flight), `is_empty(&self) -> bool`.
  - `shutdown(&self, grace: Duration) -> Vec<ConcludedCall>`: marks closing; drops every waiting `CatchUp` job silently; takes every waiting `Live` and `Saved` job (retries included) off the queue; waits for the job in flight up to `0.8 × grace`; a `Live` or `Saved` job still in flight at the deadline is marked answered (its late end sends nothing) and added to the result; returns the calls to save, in queue order. No `call.result` is sent for any of them.

- [ ] **Step 1: Write the failing tests** (each with `testing::capture()`, `retry_after` of 20 ms, and a `work` closure keyed on `call.call.talkgroup`; calls from `testing::call`)

```rust
#[test] fn the_front_lane_goes_before_catch_up() {
    // work blocks the worker on a gate until released and records (kind, tg) in start order; push CatchUp 1, CatchUp 2, Live 3, Saved 4, release.
    // Expect start order: 1 (already in flight), 3, 4, 2.
}
#[test] fn results_go_back_for_live_only() {
    // tg 1 → Ok{took, bytes: 64}, tg 2 → Skipped("analog"), tg 3 → Failed("no frames"); each pushed Live; tg 4 as CatchUp → Ok, tg 5 as Saved → Ok.
    // out.results() == [(p1, Ok, "", ""), (p2, Skipped, "analog", ""), (p3, Failed, "no frames", "")]; the Ok result carries took_ms; snapshot: ok 3, failed 1, catch_up_ok 1, bytes 192.
}
#[test] fn retries_then_gives_up() {
    // work counts calls and always returns Retry("disk"): 4 calls (1 + 3 retries), result Failed "gave up: disk", snapshot.retries == 3, three warn logs "retry 1/3 …" … "retry 3/3 …".
}
#[test] fn a_duplicate_path_is_queued_once_and_a_live_push_makes_it_live() {
    // block the worker on another job; push c as CatchUp → true; push c as CatchUp again → false; push c as Live → true; release; c runs once and sends one call.result Ok.
    // Then: push d as Live twice → the second is false and d gets exactly one result.
}
#[test] fn shutdown_returns_what_to_save_and_sends_nothing() {
    // block the worker on live job 1 (not released until the end); push Live 2, Saved 3, CatchUp 4; shutdown(100 ms).
    // Expect: it returns the calls of 2, 3 (in that order) and 1 (in flight at the deadline), nothing for 4; out.results() is empty; it returned within 150 ms.
    // Release job 1: it finishes, and results() is still empty (its late end sends nothing).
    // Push Live 5 after the shutdown → results() == [(p5, Failed, "shutting down", "")].
}
#[test] fn a_panicking_job_is_failed_not_fatal() {
    // work panics for tg 1 and is Ok for tg 2: results [(p1, Failed, "the work panicked", ""), (p2, Ok, …)].
}
```

- [ ] **Step 2: Run them to see them fail.**
- [ ] **Step 3: Implement `queue.rs`** from the transcribe one: remove its queue-full rule, age rule, engine up/down bookkeeping (`in_a_row`, `down_since`) and the `failed` callback; add `Saved`, `bytes`, `catch_up_ok`, and the saving `shutdown`. The `Flight` entry keeps the job's `ConcludedCall` so an unfinished one can be returned.
- [ ] **Step 4: `cargo test queue` → pass (run three times: they must not flake); clippy and fmt clean.**
- [ ] **Step 5: Commit** — `git commit -am "The work queue: live and saved calls first, catch-up behind, retries, a shutdown that hands back what to save"`

---

### Task 6: The work — one job from frames file to `.dvcf`

**Files:**
- Create: `src/work.rs`
- Modify: `src/lib.rs`
- Test: `src/work.rs`

**Interfaces:**
- Consumes: `frames::{find, read, Source}`, `dvcf::{build, Provenance}`, `output::{target, write_atomic}`, `queue::{Job, Kind, Done}`, `config::{Config, SystemConfig}`.
- Produces: `pub struct SystemView { pub short_name: String, pub enabled: bool }` and `pub fn systems_of(setup: &Setup<Config, SystemConfig>) -> Vec<SystemView>` (`enabled` = `s.config.as_ref().is_none_or(|c| c.enabled)`).
- Produces: `pub struct Ctx { pub config: Arc<RwLock<Config>>, pub systems: Arc<RwLock<Vec<SystemView>>>, pub capture_dir: PathBuf, pub host_version: String, pub skipped: Arc<Mutex<BTreeMap<String, u64>>> }` (`Clone`).
- Produces: `pub const NO_FRAMES: &str = "no frames file: turn on captureFrames in Recording settings"`.
- Produces: `pub fn skip_reason(ctx: &Ctx, call: &ConcludedCall) -> Option<String>`: in order: the system (by `short_name`) is listed and disabled → `"system disabled"`; `audio_type == "analog"` → `"analog"`; `voice_mode == "provoice"` → `"ProVoice"`; `find(..)` is `None` → `NO_FRAMES`; else `None`. (A system not in the list counts as enabled.)
- Produces: `pub fn count_skip(ctx: &Ctx, reason: &str)`: `skipped[reason] += 1`.
- Produces: `pub fn process(ctx: &Ctx, job: &Job, warn: &dyn Fn(String)) -> Done`:
  1. `skip_reason` → `Done::Skipped(reason)` (and `count_skip`; a `CatchUp` job never reaches here with a reason other than a frames file gone meanwhile, which is fine).
  2. `target(..)`: for `Kind::CatchUp`, if it exists → `Done::Skipped("already written")` without counting.
  3. `read` → `Err` → `Done::Retry(e)`.
  4. `build` → `Err("no frames")` → `Done::Failed("no frames in <file name>")`; each `warning` → `warn("<path>: <warning>")`.
  5. `write_atomic` → `Err(e)` → `Done::Retry(e)`; else `Done::Ok { took, bytes }` where `took` is measured from the start of step 3.
  The `Ok` message sent to the recorder is empty; the recorder's `url` field is empty too.

- [ ] **Step 1: Write the failing tests**

```rust
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
```

(`ctx_for(dir, systems)` and `job(call, kind)` are test helpers in the module: `Ctx` with `Config::default()`, `capture_dir = dir`, `host_version = "0.3.0"`; `Job { call, kind, enqueued: Instant::now(), tries: 0 }`.)

- [ ] **Step 2: Run `cargo test work` to see them fail.**
- [ ] **Step 3: Implement `src/work.rs`.** `Provenance.plugin_version` is `env!("CARGO_PKG_VERSION")`.
- [ ] **Step 4: `cargo test work` → pass; clippy and fmt clean.**
- [ ] **Step 5: Commit** — `git commit -am "Work: the skip rules, then frames file to .dvcf, live overwriting and catch-up never"`

---

### Task 7: The catch-up pass

**Files:**
- Create: `src/catchup.rs` (start from `D:\trunk-plugin-transcribe\src\archive.rs`: keep its `Pass` / `begin` / `on_page` / `check_timeout` / `finish` shape and its tests' style)
- Modify: `src/lib.rs`
- Test: `src/catchup.rs`

**Interfaces:**
- Consumes: `queue::{Queue, Kind}`, `work::{Ctx, skip_reason}`, `output::target`, `frames::find`, `trunk_recorder_plugin::{CallsQuery, CallsPage, StoredCall, ConcludedCall, CallFiles, Host}`.
- Produces: `pub enum Mode { Start, Periodic }`; `pub const PAGE: usize = 500`; `pub const BUDGET: usize = 2000`; `pub const PAGE_TIMEOUT: Duration = 60 s`.
- Produces: `pub struct CatchUp` with `CatchUp::new(host: Host, ctx: Ctx, queue: Arc<Queue>, budget: usize) -> CatchUp` (the plugin passes `BUDGET`; tests pass a small number), `running(&self) -> bool`, `begin(&mut self, mode: Mode, now_ms: i64) -> bool` (false when a pass runs; `from` = `now_ms − catchUpDays × 86 400 000`, `None` when `catchUpDays` is 0, read from `ctx.config` at begin; logs `info` `catch-up started (start|periodic)`; `stop_early` = `Periodic` and the last pass did not pause), `on_page(&mut self, request: u64, page: CallsPage) -> bool`, `check_timeout(&mut self, now: Instant) -> bool`, `store_error(&self) -> Option<String>` (the last pass's page `error`, cleared by a pass that ends without one), `queued_total(&self) -> u64` (run total of jobs queued by passes).
- Behaviour per page entry, in order: `skip_reason` on a `ConcludedCall { path, system: index by short_name else 0, call: record, files: CallFiles { json: <capture_dir>/<path>.json, wav: <capture_dir>/<path>.wav, m4a: None } }` → counted as skipped, not queued (the `NO_FRAMES` reason is the common one: the frames file was cleaned up); the target `.dvcf` exists → settled; else `queue.push(call, Kind::CatchUp)`. When `queue.catch_up_waiting() >= BUDGET` the pass pauses: logs `info` `catch-up paused: <n> calls queued, the rest waits for the next pass`, remembers it, ends. A page `error` → `warn` `catch-up: the recorder's call store answered: <e>` (once per pass), remembered for `store_error`, the pass ends. The end of every pass logs `info` `catch-up: <queued> queued, <skipped> skipped, <settled> already written, <pages> pages`.

- [ ] **Step 1: Write the failing tests** (a fixture like archive.rs's: `testing::capture()`, a queue whose `work` records what it saw and answers `Ok`, calls made with `fixtures::sdr_call` so their frames files exist under `dir`, `StoredCall { path, record, audio: true, json: true }` from each)

```rust
#[test] fn a_start_pass_queues_what_has_frames_and_no_dvcf_and_pages_to_the_end() {
    // calls a (frames, no dvcf), b (frames, a .dvcf already beside it), c (no frames file), d (frames, analog record): page 1 = [a, b] with next "500", page 2 = [c, d] with no next.
    // Expect: one CallsQuery with from = now − 30 days and limit 500; after page 1 a second query with cursor "500" and the same from; after page 2 the pass is over; the worker saw exactly [CatchUp a]; info logs contain "catch-up started (start)" and "catch-up: 1 queued, 2 skipped, 1 already written, 2 pages".
}
#[test] fn a_periodic_pass_stops_at_the_first_page_with_nothing_queued() { /* page 1 = [b (dvcf exists)] with next: no second query; "0 queued, 0 skipped, 1 already written, 1 pages" */ }
#[test] fn zero_days_means_no_lower_bound() { /* catch_up_days 0 → the query's from is None */ }
#[test] fn the_store_being_off_ends_the_pass_and_is_remembered() {
    // page error "the call store is off": one warn "catch-up: the recorder's call store answered: the call store is off", pass over, store_error() == Some(..); a later pass that ends cleanly clears it.
}
#[test] fn a_pass_queues_up_to_the_budget_and_the_next_periodic_one_walks_on() {
    // a queue whose worker is blocked, a CatchUp with budget 2: a page of 5 convertible calls queues 2, logs "catch-up paused: 2 calls queued, the rest waits for the next pass", asks no further page; the next Periodic pass does not stop early on a page with nothing queued, and the one after it does.
}
#[test] fn a_page_that_never_comes_ends_the_pass() { /* check_timeout at +61 s: warn "catch-up: no answer from the recorder's call store in 60 s"; a late page is not the pass's */ }
#[test] fn a_second_begin_while_running_is_refused_and_a_foreign_page_ignored() { /* as archive.rs's */ }
```

- [ ] **Step 2: Run `cargo test catchup` to see them fail.**
- [ ] **Step 3: Implement `src/catchup.rs`.**
- [ ] **Step 4: `cargo test catchup` → pass; clippy and fmt clean.**
- [ ] **Step 5: Commit** — `git commit -am "Catch-up: pages the call store newest first and queues what still has frames and no .dvcf"`

---

### Task 8: The saved queue

**Files:**
- Create: `src/saved.rs`
- Modify: `src/lib.rs`
- Test: `src/saved.rs`

**Interfaces:**
- Produces: `pub const FILE: &str = "queue.jsonl"`.
- Produces: `pub fn save(data_dir: &Path, calls: &[ConcludedCall]) -> Result<(), String>`: with an empty slice removes `<data_dir>/queue.jsonl` if present; otherwise writes one `serde_json::to_string(call)` per line, atomically (`.tmp` then rename).
- Produces: `pub fn load(data_dir: &Path, warn: &dyn Fn(String)) -> Vec<ConcludedCall>`: reads and removes the file; a line that does not parse is one `warn` `queue.jsonl line <n>: <why>` and is skipped; no file → empty, no warning.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn saved_calls_come_back_in_order_and_the_file_is_gone() {
    // save three testing::call()s; load → the same three (PartialEq on path and call.talkgroup); the file no longer exists; a second load is empty.
}
#[test] fn an_empty_save_removes_the_file_and_a_bad_line_is_skipped_with_a_warning() {
    // save one, then save(&[]) → no file; write "{\"path\": 1}\n<good line>\n" by hand → load gives the good one and one warning starting "queue.jsonl line 1: ".
}
```

- [ ] **Step 2: Run `cargo test saved` to see them fail.**
- [ ] **Step 3: Implement `src/saved.rs`.**
- [ ] **Step 4: `cargo test saved` → pass; clippy and fmt clean.**
- [ ] **Step 5: Commit** — `git commit -am "Saved queue: the live calls still waiting at shutdown, kept for the next start"`

---

### Task 9: The plugin — wiring, live settings, upkeep, shutdown

**Files:**
- Modify: `src/plugin.rs` (replace the stub), `src/lib.rs`
- Test: `src/plugin.rs`

**Interfaces:**
- Consumes: everything above. Constants: `TICK = 10 s`, `CATCH_UP_EVERY = 600 s`, `RETAIN_FIRST = 60 s`, `RETAIN_EVERY = 3600 s`, `RETRY_AFTER = [1 s, 5 s, 15 s]`.
- Produces: `pub struct Dvcf` implementing `Plugin` with `Config = config::Config`, `SystemConfig = config::SystemConfig`:
  - `start`: validate; if `outputDir` is set, `create_dir_all` it (failure → status `error` `cannot create the output folder <dir>: <e>`, remembered, the plugin still starts); build `Ctx`, the `Queue` (work = `work::process`), `CatchUp`; `saved::load` and push each as `Kind::Saved` when `find` still finds its frames file, else `info` `saved call <path> dropped: its frames file is gone`; `info` `dvcf <version>: <"writing beside the calls" | "writing to <dir>">`; if `catchUp`, `begin(Mode::Start)`; start the upkeep thread.
  - `call_concluded`: `skip_reason` → `call_result(path, Skipped, reason)` at once (and `count_skip`; for `NO_FRAMES` also set the `no_frames` flag that the status shows until a call with a frames file arrives); else `queue.push(call, Live)`, and when that returns false `call_result(path, Skipped, "already queued")`.
  - `calls`: `catchup.on_page`.
  - `settings`: validate (refused → status `error` `new settings not taken: <e>` until good ones come, `Err`); if `outputDir` changed and is set, `create_dir_all` as at start; swap `ctx.config` and `ctx.systems`; `Ok`.
  - `shutdown(grace)`: stop the upkeep thread; wait for the queue to empty up to `0.5 × grace`; `queue.shutdown(remaining)` → `saved::save(data_dir, &calls)` (an error → `warn`); `info` `<n> live calls saved for the next start` when n > 0; send metrics and status once more.
  - the upkeep thread's tick: `catchup.check_timeout`; every `CATCH_UP_EVERY` with `catchUp` on, `begin(Mode::Periodic)`; retention at `RETAIN_FIRST` then every `RETAIN_EVERY` when `retentionDays > 0`: `output::sweep(root, days × 86 400 s)` on `outputDir` or the capture folder, logging `info` `<n> .dvcf files older than <d> days deleted` when n > 0; then `metrics` and `status`.
  - `fn metrics_of(snap: &Snapshot, ctx: &Ctx, catch_up_queued: u64) -> Metrics` and `fn status_of(refused: Option<&str>, output_error: Option<&str>, no_frames: bool, store_error: Option<&str>, where_: &str) -> (State, String)`: `error` refused → `error` output → `warning` `NO_FRAMES` → `warning` `call store: <e>` → `ok` `where_`.

- [ ] **Step 1: Write the failing tests** (through `testing::run::<Dvcf>`; calls from `fixtures`)

```rust
#[test] fn a_live_call_is_written_and_answered() {
    // hello + CallConcluded(sdr_call P25Fdma): results == [(path, Ok, "", "")] with took_ms; the .dvcf exists beside the JSON and read::messages parses it; exactly one CallsQuery (the start pass); the last status is (Ok, "writing beside the calls"); metrics.last().extra["written"] == 1.
}
#[test] fn calls_without_frames_are_skipped_and_the_status_says_why() {
    // a testing::call() (no frames file) → (Skipped, NO_FRAMES); status (Warning, NO_FRAMES); then an sdr_call → Ok and the status back to Ok. Also an analog record → (Skipped, "analog") and a disabled system (hello.systems[0].config = {"enabled": false}) → (Skipped, "system disabled"); metrics extra["skipped"] == {"analog": 1, "system disabled": 1, NO_FRAMES: 1} over one run.
}
#[test] fn a_call_concluded_twice_gets_one_answer_each() {
    // the same sdr_call twice in one run; with the worker that fast the second may run after the first finished: assert results are two entries for the path, each Ok or (Skipped, "already queued"), and the file exists.
}
#[test] fn a_page_from_the_store_is_converted_without_a_result() {
    // hello with {"catchUp": true} + Calls { request: 1, page: [StoredCall of an sdr_call] }: the .dvcf appears; results() is empty; info log contains "catch-up: 1 queued".
}
#[test] fn catch_up_off_asks_for_nothing() { /* hello {"catchUp": false}: no CallsQuery in the output */ }
#[test] fn new_settings_are_taken_at_once() {
    // hello; Settings with outputDir = <temp out>; CallConcluded(sdr_call) → the file is under out, not beside the JSON; Settings with outputDir "relative" → status (Error, "new settings not taken: Output folder: must be an absolute path").
}
#[test] fn shutdown_saves_the_queue_and_the_next_start_works_it_silently() {
    // By hand (testing::capture + Dvcf::start, catchUp off): three sdr_calls; before the first is concluded, make a folder at its target (`files.json.with_extension("dvcf")`) so its write fails and it waits for a retry (1 s). Conclude all three; wait until results() has two Ok entries (calls 2 and 3; the worker is free while 1 waits); shutdown(1 s).
    // Expect: queue.jsonl in data_dir holds exactly call 1; results() has nothing for call 1; an info log "1 live calls saved for the next start".
    // Remove the folder; start again with the same data_dir (testing::run with just the hello): afterwards queue.jsonl is gone, call 1's .dvcf exists, and results() is empty.
}
#[test] fn retention_sweeps_only_when_set() {
    // Unit-test the tick: an Upkeep with next_retain in the past, retentionDays 7, an old .dvcf and a fresh one under the capture folder → one deleted, info log "1 .dvcf files older than 7 days deleted"; with retentionDays 0 nothing is deleted.
}
#[test] fn the_status_rules_in_order() {
    // status_of: refused > output error > no frames > store error > ok with the place.
}
```

- [ ] **Step 2: Run `cargo test plugin` to see them fail.**
- [ ] **Step 3: Implement `src/plugin.rs`** on the transcribe plugin's shape (`Report`, `Upkeep`, the upkeep thread with its stop channel, `lock` / `read` / `write` helpers).
- [ ] **Step 4: `cargo test` (the whole crate) → pass; clippy and fmt clean. Run `cargo test` three times for flakes.**
- [ ] **Step 5: Commit** — `git commit -am "The plugin: live calls, catch-up, retention, live settings, a shutdown that saves its queue"`

---

### Task 10: Documentation, example settings and release readiness

**Files:**
- Modify: `README.md`, `CHANGELOG.md`
- Create: `examples/settings.json`

- [ ] **Step 1: Write the README** for a user: what it does (one paragraph: one `.dvcf` per digital call, from TRP's frame capture, everything but the audio inside); the prerequisite in bold (`captureFrames` on in Recording settings, and what happens without it: every call skipped with the reason on the plugin's card); which calls get a file (P25 Phase 1 and 2, DMR, NXDN; not analog or ProVoice); the settings table (every key, default, one line each); where the files go and the retention rule; catch-up (what it does, that it never overwrites); what the file holds, with a pointer to `DVCF_SPEC.md` revision 2.1 in tr-plugin-dvcf and the two TRP keys; the AMBE note (the four information words, as the C++ plugin writes); "Restart" (the queue saved to `plugin-data/dvcf/queue.jsonl`); licence (GPL-3.0-or-later, why: trunk-core).
- [ ] **Step 2: `CHANGELOG.md`** `## [0.1.0]` — the features in five lines.
- [ ] **Step 3: `examples/settings.json`**: the four keys with their defaults.
- [ ] **Step 4: Verify the manifest and a release build**

Run: `cargo build --release && ./target/release/dvcf --describe > manifest.json && python .github/check_manifest.py manifest.json --release && ./target/release/dvcf --version`
Expected: no error; `0.1.0`.

- [ ] **Step 5: Commit** — `git commit -am "README, changelog and an example settings file for 0.1.0"`

---

### Task 11: The spec revision for tr-plugin-dvcf (a branch, ready for a pull request)

**Files:**
- Create (in a scratch clone, not this repo): `DVCF_SPEC.md` edits on a branch `spec-2.1` of `https://github.com/trunk-reporter/tr-plugin-dvcf`, cloned to `D:\tr-plugin-dvcf` (`gh repo clone trunk-reporter/tr-plugin-dvcf D:\tr-plugin-dvcf`).

- [ ] **Step 1: Make the four edits** to `DVCF_SPEC.md`:
  1. The title line: `# SymbolStream Protocol v2` → `# SymbolStream Protocol v2 (document revision 2.1)` and, under it, one line: `Revision 2.1 changes the document only: the wire `version` byte stays 2.`
  2. §3.6: after the `src_list` table, a paragraph "Keys written by Trunk Recorder Pro's `dvcf` plugin" with a two-row table: `trunk_recorder_call` (object: the recorder's complete call record, exactly as its `.json` file holds it; see TRP's call JSON documentation) and `trunk_recorder_pro` (object: `version` the recorder's, `plugin` e.g. `dvcf 0.1.0`, `frames_file` `sdr` or `frames.jsonl`). Both optional; readers ignore unknown keys as §3.6 already says.
  3. §7.2: replace the table and the "channel data bytes" text with: `params[0]` u0, 12 bits; `params[1]` u1, 12 bits; `params[2]` u2, 11 bits; `params[3]` u3, 14 bits — the four AMBE+2 information words after FEC, 49 bits in all, each right-aligned in its uint32 (low bits significant), MSB first within the word; as trunk-recorder's `voice_codec_data()` callback hands them and as both the C++ plugin and TRP's `dvcf` plugin have always written. Remove the reconstruction example that packed bytes. Add: "Files written before this revision also hold these words: the earlier text described the 72-bit channel frame, which no writer produced."
  4. §6: after the registry table, "Trunk Recorder Pro produces types 0, 1, 2 and 6 only." §9.1: after the structure list, "`flags` bit 0 of a CODEC_FRAME marks a frame the recorder's vocoder did not play as received (lost, repeated or muted); its codec words are a reconstruction."
- [ ] **Step 2: Run the fixture scripts still pass**: `python -I scripts/validate_dvcf_fixture.py <a .dvcf from this plugin's tests>` → `ok`.
- [ ] **Step 3: Commit on the branch** — `git commit -am "Spec revision 2.1: TRP's metadata keys, the AMBE payload as written, which types TRP produces, the silence flag"` — and stop. Opening the pull request is done after the plan (below), not by the implementer.

---

## After the plan (not tasks for the implementer)

- **Live run** under a #104-based recorder with `captureFrames` on: install the build through **Install from file**, watch the start pass convert the last 30 days, check one file with `scripts/validate_dvcf_fixture.py` and with IMBE-ASR's reader.
- **The spec pull request** from `D:\tr-plugin-dvcf`'s `spec-2.1` branch to `trunk-reporter/tr-plugin-dvcf` (the user's org; opened with the user).
- Create `trunk-reporter/trunk-plugin-dvcf`, push `main`, tag `v0.1.0` once the SDK 0.9.0 recorder is released; registry submission after.
