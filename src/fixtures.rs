//! Test fixtures: calls with frames files made by trunk-core's own writers, so the tests prove the round trip against TRP's real encoder.

use std::path::Path;

use trunk_core::mbe::Kind;
use trunk_core::trunk::frames::{Codec, VoiceFrame, frames_jsonl};
use trunk_core::trunk::sdr::{SdrInfo, SdrMode, call_sdr};
use trunk_recorder_plugin::testing;
use trunk_recorder_plugin::{ConcludedCall, SrcEntry};

/// `n` frames of `codec`, each different, with values the `.sdr` keeps (`errs` and `e0` under 32).
pub fn frames(codec: Codec, n: usize) -> Vec<VoiceFrame> {
    let bits = if codec == Codec::Imbe { 88 } else { 49 };
    (0..n)
        .map(|k| VoiceFrame {
            codec,
            bits: (0..bits).map(|i| u8::from((k * 7 + i * 3) % 5 == 0)).collect(),
            e0: if codec == Codec::Imbe { (k % 4) as u32 } else { 0 },
            errs: (k % 6) as u32,
            erased: k == 3 && codec == Codec::Imbe,
            kind: match k {
                1 => Kind::Repeat,
                2 => Kind::Muted,
                4 if codec == Codec::Ambe => Kind::Tone,
                _ => Kind::Voice,
            },
            tone: None,
        })
        .collect()
}

/// A call on talkgroup `tg` with `n` frames of `mode`'s codec in an `.sdr` beside its JSON; the frames it was made from.
pub fn sdr_call(dir: &Path, mode: SdrMode, tg: u32, n: usize) -> (ConcludedCall, Vec<VoiceFrame>) {
    let (voice_mode, codec) = match mode {
        SdrMode::P25Fdma => ("p25", Codec::Imbe),
        SdrMode::P25Tdma => ("p25_phase2", Codec::Ambe),
        SdrMode::Dmr => ("dmr", Codec::Ambe),
    };
    let call = digital_call(dir, tg, voice_mode, n);
    let frames = frames(codec, n);
    let info = SdrInfo {
        start_us: call.call.start_time_ms as u64 * 1000,
        duration_ms: call.call.call_length_ms as u32,
        hz: call.call.freq as u32,
        tg,
        ..Default::default()
    };
    let sdr = call_sdr(mode, &frames, &info).expect("an sdr of one codec");
    std::fs::write(call.files.json.with_extension("sdr"), sdr).expect("sdr");
    (call, frames)
}

/// A call on talkgroup `tg` (NXDN) with `frames` in a `.frames.jsonl` beside its JSON.
pub fn jsonl_call(dir: &Path, tg: u32, frames: &[VoiceFrame]) -> ConcludedCall {
    let call = digital_call(dir, tg, "nxdn48", frames.len());
    std::fs::write(call.files.json.with_extension("frames.jsonl"), frames_jsonl(frames)).expect("frames.jsonl");
    call
}

/// `testing::call` made a digital call of `voice_mode` and `n` frames, with two talkers, its JSON rewritten to match.
fn digital_call(dir: &Path, tg: u32, voice_mode: &str, n: usize) -> ConcludedCall {
    let mut call = testing::call(dir, "sys1", tg);
    let r = &mut call.call;
    r.voice_mode = voice_mode.into();
    r.call_num = tg as u64;
    r.start_time_ms = r.start_time * 1000 + 250;
    r.call_length_ms = n as i64 * 20;
    let second = (n / 2) as f64 * 0.02;
    r.src_list = vec![
        SrcEntry { src: 1234, time: r.start_time, pos: 0.0, ..Default::default() },
        SrcEntry { src: 5678, time: r.start_time + second as i64, pos: second, ..Default::default() },
    ];
    std::fs::write(&call.files.json, call.call.to_json()).expect("json");
    call
}
