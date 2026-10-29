//! The DVCF builder: a call's record and vocoder frames as one SymbolStream v2 `.dvcf` file's bytes.
//!
//! Every message is `SY`, version 2, a kind byte and a little-endian payload length, then the payload. A call is one CALL_START, a
//! CODEC_FRAME per vocoder frame, one CALL_METADATA carrying the whole call record as JSON, and one CALL_END, in the layouts of
//! tr-plugin-dvcf's `DVCF_SPEC.md`, so the classic trunk-recorder plugin's readers read ours too.

use serde_json::{Map, Value, json};
use trunk_core::mbe::Kind;
use trunk_core::p25::voice::imbe_bits_to_params;
use trunk_core::trunk::frames::{Codec, VoiceFrame};
use trunk_recorder_plugin::CallRecord;

use crate::frames::Source;

const CODEC_FRAME: u8 = 0x01;
const CALL_START: u8 = 0x02;
const CALL_END: u8 = 0x03;
const CALL_METADATA: u8 = 0x05;

/// A frame's length in time: each vocoder frame is 20 ms.
const FRAME_US: u64 = 20_000;

/// Where a file came from, for `trunk_recorder_pro` in its metadata.
#[derive(Clone, Copy, Debug)]
pub struct Provenance<'a> {
    /// Trunk Recorder Pro's version.
    pub host_version: &'a str,
    /// This plugin's version.
    pub plugin_version: &'a str,
    /// The kind of frames file the frames were read from.
    pub source: Source,
}

/// A built `.dvcf`: its bytes, how many frames it holds, and anything worth a warning in the log.
#[derive(Debug)]
pub struct Built {
    pub bytes: Vec<u8>,
    pub frames: usize,
    pub warnings: Vec<String>,
}

/// The messages of a file, written one after another.
#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// One message of `kind` around `payload`.
    fn message(&mut self, kind: u8, payload: &[u8]) {
        self.bytes.extend_from_slice(b"SY");
        self.bytes.push(2);
        self.bytes.push(kind);
        put_u32(&mut self.bytes, payload.len() as u32);
        self.bytes.extend_from_slice(payload);
    }
}

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// A name as a length byte and its bytes, cut to 255 bytes at a character boundary.
fn put_name(b: &mut Vec<u8>, name: &str) {
    let mut end = name.len().min(255);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    b.push(end as u8);
    b.extend_from_slice(&name.as_bytes()[..end]);
}

/// A source unit's ID as the file holds it: a negative or too-large one is unknown, 0.
fn unit(src: i64) -> u32 {
    u32::try_from(src).unwrap_or(0)
}

/// The talker at `pos` seconds into the call: the `srcList` entry with the greatest `pos` not past it (the later on a tie); 0 when none.
fn talker_at(record: &CallRecord, pos: f64) -> u32 {
    let mut best: Option<(f64, i64)> = None;
    for s in record.src_list.iter().filter(|s| s.pos <= pos) {
        if best.is_none_or(|(p, _)| s.pos >= p) {
            best = Some((s.pos, s.src));
        }
    }
    best.map_or(0, |(_, src)| unit(src))
}

/// A frame's codec parameters: IMBE's eight words, or AMBE's 49 bits as four words of 12, 12, 11 and 14 bits, MSB first.
fn params(frame: &VoiceFrame) -> Result<Vec<u32>, String> {
    match frame.codec {
        Codec::Imbe => {
            let bits: &[u8; 88] = frame.bits.as_slice().try_into().map_err(|_| format!("an IMBE frame of {} bits, not 88", frame.bits.len()))?;
            Ok(imbe_bits_to_params(bits).to_vec())
        }
        Codec::Ambe => {
            if frame.bits.len() != 49 {
                return Err(format!("an AMBE frame of {} bits, not 49", frame.bits.len()));
            }
            let word = |r: std::ops::Range<usize>| frame.bits[r].iter().fold(0u32, |v, &b| v << 1 | u32::from(b & 1));
            Ok(vec![word(0..12), word(12..24), word(24..35), word(35..49)])
        }
    }
}

/// The SymbolStream codec type of AMBE frames under `voice_mode`, and whether the mode is one AMBE is not expected under.
fn ambe_type(voice_mode: &str) -> (u8, bool) {
    match voice_mode {
        "p25_phase2" => (1, false),
        "dmr" => (2, false),
        "nxdn48" | "nxdn96" => (6, false),
        _ => (1, true),
    }
}

/// The `.dvcf` for a call: its `record` and its vocoder `frames`, in order. An error when there are no frames.
pub fn build(record: &CallRecord, frames: &[VoiceFrame], p: Provenance) -> Result<Built, String> {
    if frames.is_empty() {
        return Err("no frames".into());
    }
    let start_us = if record.start_time_ms > 0 { record.start_time_ms as u64 * 1000 } else { (record.start_time.max(0) as u64).saturating_mul(1_000_000) };
    let duration_ms = if record.call_length_ms > 0 { record.call_length_ms as u32 } else { (record.call_length * 1000.0).round() as u32 };
    let tg = record.talkgroup;
    let call_id = record.call_num as u32;
    let mut w = Writer::default();
    let mut warnings = Vec::new();

    let mut b = Vec::new();
    put_u32(&mut b, tg);
    put_u64(&mut b, record.freq);
    put_u64(&mut b, start_us);
    put_u32(&mut b, call_id);
    put_name(&mut b, &record.short_name);
    w.message(CALL_START, &b);

    let (ambe, unexpected) = ambe_type(&record.voice_mode);
    let mut error_count = 0u32;
    for (k, f) in frames.iter().enumerate() {
        let codec_type = match f.codec {
            Codec::Imbe => 0,
            Codec::Ambe => {
                if unexpected && warnings.is_empty() {
                    warnings.push(format!("AMBE frames under voice mode \"{}\": written as P25 Phase 2 (type 1)", record.voice_mode));
                }
                ambe
            }
        };
        let params = params(f).map_err(|why| format!("frame {k}: {why}"))?;
        let errs = if f.erased { 0 } else { f.errs.min(255) as u8 };
        let silent = f.erased || matches!(f.kind, Kind::Repeat | Kind::Muted | Kind::Erasure);
        error_count = error_count.saturating_add(u32::from(errs));
        let mut b = Vec::with_capacity(24 + 4 * params.len());
        put_u32(&mut b, tg);
        put_u32(&mut b, talker_at(record, k as f64 * 0.02));
        put_u32(&mut b, call_id);
        put_u64(&mut b, start_us + k as u64 * FRAME_US);
        b.extend_from_slice(&[codec_type, params.len() as u8, errs, u8::from(silent)]);
        params.iter().for_each(|&v| put_u32(&mut b, v));
        w.message(CODEC_FRAME, &b);
    }

    let json = serde_json::to_vec(&metadata(record, p)).map_err(|e| format!("metadata: {e}"))?;
    w.message(CALL_METADATA, &json);

    let mut b = Vec::new();
    put_u32(&mut b, tg);
    put_u32(&mut b, call_id);
    put_u32(&mut b, record.src_list.last().map_or(0, |s| unit(s.src)));
    put_u64(&mut b, record.freq);
    put_u32(&mut b, duration_ms);
    put_u32(&mut b, error_count);
    b.push(u8::from(record.encrypted));
    put_name(&mut b, &record.short_name);
    w.message(CALL_END, &b);

    Ok(Built { bytes: w.bytes, frames: frames.len(), warnings })
}

/// CALL_METADATA's JSON object for a call: the C++ plugin's columns first (the talkgroup's strings only when set), then the whole
/// record as `trunk_recorder_call` and where the file came from as `trunk_recorder_pro`.
pub fn metadata(record: &CallRecord, p: Provenance) -> Value {
    let mut m = Map::new();
    for (key, value) in [
        ("tg_tag", &record.talkgroup_group_tag),
        ("tg_alpha_tag", &record.talkgroup_tag),
        ("tg_group", &record.talkgroup_group),
        ("tg_description", &record.talkgroup_description),
    ] {
        if !value.is_empty() {
            m.insert(key.into(), json!(value));
        }
    }
    m.insert("signal".into(), json!(record.signal));
    m.insert("noise".into(), json!(record.noise));
    m.insert("freq_error".into(), json!(record.freq_error));
    m.insert("spike_count".into(), json!(record.spike_count()));
    m.insert("emergency".into(), json!(record.emergency));
    m.insert("priority".into(), json!(record.priority));
    m.insert("phase2_tdma".into(), json!(record.phase2_tdma));
    m.insert("tdma_slot".into(), json!(record.tdma_slot));
    if !record.patched_talkgroups.is_empty() {
        m.insert("patched_tgs".into(), json!(record.patched_talkgroups));
    }
    if !record.src_list.is_empty() {
        let list = record
            .src_list
            .iter()
            .map(|s| json!({ "src": s.src, "time": s.time, "pos": s.pos, "emergency": u8::from(s.emergency), "signal_system": s.signal_system, "tag": s.tag }));
        m.insert("src_list".into(), Value::Array(list.collect()));
    }
    m.insert("trunk_recorder_call".into(), serde_json::from_str(&record.to_json()).unwrap_or(Value::Null));
    m.insert("trunk_recorder_pro".into(), json!({ "version": p.host_version, "plugin": format!("dvcf {}", p.plugin_version), "frames_file": p.source.name() }));
    Value::Object(m)
}

/// A reader for the tests: a `.dvcf` file's bytes back as messages.
#[cfg(test)]
pub mod read {
    use serde_json::Value;

    /// One SymbolStream v2 message, its fields as written.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Msg {
        Start { tg: u32, freq: u64, ts: u64, call_id: u32, name: String },
        Frame { tg: u32, src: u32, call_id: u32, ts: u64, codec: u8, errs: u8, flags: u8, params: Vec<u32> },
        Metadata(Value),
        End { tg: u32, call_id: u32, src: u32, freq: u64, dur_ms: u32, errs: u32, enc: bool, name: String },
    }

    /// A cursor over one message's payload; panics when it runs short.
    struct Cursor<'a>(&'a [u8]);

    impl Cursor<'_> {
        fn take(&mut self, n: usize) -> &[u8] {
            assert!(self.0.len() >= n, "payload truncated");
            let (a, b) = self.0.split_at(n);
            self.0 = b;
            a
        }
        fn u8(&mut self) -> u8 {
            self.take(1)[0]
        }
        fn u32(&mut self) -> u32 {
            u32::from_le_bytes(self.take(4).try_into().unwrap())
        }
        fn u64(&mut self) -> u64 {
            u64::from_le_bytes(self.take(8).try_into().unwrap())
        }
        fn name(&mut self) -> String {
            let n = self.u8() as usize;
            String::from_utf8(self.take(n).to_vec()).expect("a UTF-8 name")
        }
    }

    /// Every message in `bytes`, in order. Panics on a bad header, a wrong version, a truncated payload or an unknown kind.
    pub fn messages(bytes: &[u8]) -> Vec<Msg> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            assert!(rest.len() >= 8, "header truncated");
            assert_eq!(&rest[..2], b"SY", "bad magic");
            assert_eq!(rest[2], 2, "wrong version");
            let kind = rest[3];
            let len = u32::from_le_bytes(rest[4..8].try_into().unwrap()) as usize;
            assert!(rest.len() >= 8 + len, "payload truncated");
            let mut c = Cursor(&rest[8..8 + len]);
            rest = &rest[8 + len..];
            out.push(match kind {
                0x02 => Msg::Start { tg: c.u32(), freq: c.u64(), ts: c.u64(), call_id: c.u32(), name: c.name() },
                0x01 => {
                    let (tg, src, call_id, ts, codec) = (c.u32(), c.u32(), c.u32(), c.u64(), c.u8());
                    let count = c.u8();
                    let (errs, flags) = (c.u8(), c.u8());
                    let params = (0..count).map(|_| c.u32()).collect();
                    Msg::Frame { tg, src, call_id, ts, codec, errs, flags, params }
                }
                0x05 => Msg::Metadata(serde_json::from_slice(c.0).expect("metadata JSON")),
                0x03 => {
                    Msg::End { tg: c.u32(), call_id: c.u32(), src: c.u32(), freq: c.u64(), dur_ms: c.u32(), errs: c.u32(), enc: c.u8() != 0, name: c.name() }
                }
                k => panic!("unknown message kind {k:#04x}"),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::read::{self, Msg};
    use super::*;
    use crate::fixtures;
    use crate::frames::Source;
    use serde_json::{Value, json};
    use trunk_core::p25::voice::imbe_bits_to_params;
    use trunk_core::trunk::frames::Codec;
    use trunk_core::trunk::sdr::SdrMode;
    use trunk_recorder_plugin::{CallRecord, FreqEntry, SrcEntry, testing};

    fn p() -> Provenance<'static> {
        Provenance { host_version: "0.3.0", plugin_version: "0.1.0", source: Source::Sdr }
    }
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
        let flags_errs: Vec<(u8, u8)> = m[1..13]
            .iter()
            .map(|f| match f {
                Msg::Frame { flags, errs, .. } => (*flags, *errs),
                _ => panic!(),
            })
            .collect();
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
        let codecs: Vec<(u8, usize)> = read::messages(&built.bytes)
            .iter()
            .filter_map(|m| match m {
                Msg::Frame { codec, params, .. } => Some((*codec, params.len())),
                _ => None,
            })
            .collect();
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
        assert_eq!(
            keys,
            [
                "tg_tag",
                "tg_alpha_tag",
                "tg_group",
                "signal",
                "noise",
                "freq_error",
                "spike_count",
                "emergency",
                "priority",
                "phase2_tdma",
                "tdma_slot",
                "patched_tgs",
                "src_list",
                "trunk_recorder_call",
                "trunk_recorder_pro"
            ]
        );
        assert_eq!((&m["tg_tag"], &m["tg_alpha_tag"], &m["tg_group"]), (&json!("Fire Dispatch"), &json!("BUTCO FD"), &json!("Fire")));
        assert_eq!(
            (&m["signal"], &m["noise"], &m["freq_error"], &m["spike_count"], &m["priority"]),
            (&json!(-42.5), &Value::Null, &json!(12), &json!(2), &json!(3))
        );
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
        let srcs: Vec<u32> = read::messages(&build(&r, &frames, p()).unwrap().bytes)
            .iter()
            .filter_map(|m| match m {
                Msg::Frame { src, .. } => Some(*src),
                _ => None,
            })
            .collect();
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
        assert!(matches!(m[0], Msg::Start { call_id: 4, ref name, .. } if name.len() == 255));
        let errs: Vec<u8> = m
            .iter()
            .filter_map(|x| match x {
                Msg::Frame { errs, .. } => Some(*errs),
                _ => None,
            })
            .collect();
        assert_eq!(errs, [255, 255]);
        assert!(matches!(m[4], Msg::End { errs: 510, .. }));
    }

    #[test]
    fn the_c_plugins_validator_passes_the_file() {
        let python = ["python3", "python"].into_iter().find(|p| std::process::Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success()));
        let Some(python) = python else {
            eprintln!("no python: skipped");
            return;
        };
        let dir = testing::temp_dir("dvcf");
        let (call, frames) = fixtures::sdr_call(&dir, SdrMode::P25Fdma, 24, 9);
        let f = dir.join("x.dvcf");
        std::fs::write(&f, build(&call.call, &frames, p()).unwrap().bytes).unwrap();
        let out = std::process::Command::new(python).args(["-I", "scripts/validate_dvcf_fixture.py"]).arg(&f).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok: 12 messages");
    }
}
