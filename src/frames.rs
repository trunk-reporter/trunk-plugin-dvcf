//! A call's frames file: the `.sdr` or `.frames.jsonl` Trunk Recorder Pro writes beside its WAV, found and read back through trunk-core.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use trunk_core::mbe::Kind;
use trunk_core::trunk::frames::{Codec, VoiceFrame, hex_bits};
use trunk_core::trunk::sdr::sdr_frames;

/// Which kind of frames file a call has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A MimoSDR DigitalStream: P25 Phase 1, Phase 2, DMR.
    Sdr,
    /// One JSON object per frame: NXDN, and any call an `.sdr` could not be made for.
    Jsonl,
}

impl Source {
    /// Its name, as `trunk_recorder_pro.frames_file` carries it.
    pub fn name(self) -> &'static str {
        match self {
            Source::Sdr => "sdr",
            Source::Jsonl => "frames.jsonl",
        }
    }

    /// The file's extension (without the leading dot).
    pub fn extension(self) -> &'static str {
        self.name()
    }
}

/// A call's frames file: beside its JSON first (the call may be held elsewhere, as in a spool), then under the capture folder by the
/// call's key `path`; `.sdr` before `.frames.jsonl` in each place. None when there is none.
pub fn find(json: &Path, capture_dir: &Path, path: &str) -> Option<(PathBuf, Source)> {
    let sources = [Source::Sdr, Source::Jsonl];
    let beside = sources.iter().map(|&s| (json.with_extension(s.extension()), s));
    let captured = sources.iter().map(|&s| (capture_dir.join(format!("{path}.{}", s.extension())), s));
    beside.chain(captured).find(|(f, _)| f.is_file())
}

/// The frames in `file`, read as `source`.
pub fn read(file: &Path, source: Source) -> Result<Vec<VoiceFrame>, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    match source {
        Source::Sdr => Ok(sdr_frames(&bytes)),
        Source::Jsonl => String::from_utf8_lossy(&bytes)
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(k, l)| jsonl_frame(l).map_err(|why| format!("line {}: {why}", k + 1)))
            .collect(),
    }
}

/// One line of a `.frames.jsonl`, as trunk-core's `frames_jsonl` writes it.
#[derive(Deserialize)]
struct Line {
    codec: String,
    bits: String,
    e0: u32,
    errs: u32,
    erased: bool,
    out: String,
}

fn jsonl_frame(line: &str) -> Result<VoiceFrame, String> {
    let l: Line = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let (codec, n) = match l.codec.as_str() {
        "imbe" => (Codec::Imbe, 88),
        "ambe" => (Codec::Ambe, 49),
        c => return Err(format!("unknown codec \"{c}\"")),
    };
    let bits = hex_bits(&l.bits, n).ok_or_else(|| format!("bits \"{}\" are not {n} bits of hex", l.bits))?;
    let kind = match l.out.as_str() {
        "voice" => Kind::Voice,
        "repeat" => Kind::Repeat,
        "muted" => Kind::Muted,
        "erasure" => Kind::Erasure,
        "tone" => Kind::Tone,
        o => return Err(format!("unknown out \"{o}\"")),
    };
    Ok(VoiceFrame { codec, bits, e0: l.e0, errs: l.errs, erased: l.erased, kind, tone: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use trunk_core::trunk::frames::Codec;
    use trunk_core::trunk::sdr::SdrMode;
    use trunk_recorder_plugin::testing;

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
                if codec == Codec::Imbe {
                    assert_eq!((a.erased, a.e0), (b.erased, b.e0), "frame {k}");
                }
                if !b.erased {
                    assert_eq!(a.errs, b.errs, "{mode:?} frame {k}");
                }
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
}
