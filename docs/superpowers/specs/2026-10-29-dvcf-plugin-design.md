# The `dvcf` plugin: every digital call as a self-contained `.dvcf` file

**Status:** design, 2026-10-29. For Trunk Recorder Pro (TRP), plugin SDK 0.9 as PR #104 has it.

## Intent

Three research pipelines read SymbolStream v2 `.dvcf` files today: IMBE-ASR (transcription
straight from the codec words), a comparison of decoding strategies for audio quality and error
correction, and training a neural vocoder that synthesises audio from the codec words with
frequency restoration. They are fed by the classic Trunk Recorder's `mqtt_dvcf` C++ plugin
([tr-plugin-dvcf](https://github.com/trunk-reporter/tr-plugin-dvcf)), which taps the decoder's
`voice_codec_data()` callback. TRP has no such callback for plugins, so this plugin makes the same
files another way: from the vocoder frames TRP already saves beside each digital call when
`captureFrames` is on.

The point of the format is that **one file holds everything about a call** that the pipelines
want or might want, so a reader never needs the call's other files. This plugin takes that
further than the C++ one could: TRP's call record carries more than classic Trunk Recorder's, and
all of it goes in.

## Goals

- For every digital call TRP records, write one `<base>.dvcf` that a reader of the current
  spec (`DVCF_SPEC.md` in tr-plugin-dvcf) parses unchanged, with the same per-frame values the
  C++ plugin would have produced from the same air.
- Carry the whole TRP call record in the file, beside the spec's metadata fields.
- Catch up: on first run, and periodically, convert past calls that still have their frames file.
- Keep the plugin small: it's a converter with a queue, not a second recorder.

## Non-goals

- Audio in the file. The pipelines that want audio pair the `.dvcf` with the WAV by base name.
  (The spec reserves message 0x06 for an audio blob; this plugin never writes it.)
- Streaming frames live over TCP, UDP or MQTT. Files only. TRP's own MQTT plugin exists for
  anyone who wants call-end messages.
- Pre-correction bits. TRP keeps corrected information bits and per-frame error counts, the same
  as the C++ plugin's callback gives; the received bits before FEC are gone in both.
- Analog and ProVoice calls. TRP keeps no frames for them; they produce nothing.
- Reading `.dvcf` back. (A validator in the test suite does, for the tests only.)

## Anti-goals

- A `.dvcf` whose frames differ from what TRP's vocoder played. The frames must be the kept
  frames in order, bit for bit, or the vocoder-training pairs are wrong.
- A half-written file a reader can see. Every file appears whole or not at all.
- Slowing the recorder. Conversion is milliseconds, but it runs off the protocol thread anyway.

## What TRP gives the plugin

Facts about TRP at PR #104's tip (`d9b1f16`) the design rests on.

**The frames file.** With `captureFrames` on (recording settings, off by default), TRP writes
each digital call's vocoder frames beside its WAV: `<base>.sdr` for P25 Phase 1, P25 Phase 2 and
DMR; `<base>.frames.jsonl` for NXDN, which `.sdr` has no layout for. Nothing for ProVoice or
analog. The file is not listed in `call.concluded`'s `files`; the plugin finds it beside the call's
JSON file (`files.json` with its extension swapped for `.sdr` or `.frames.jsonl`), falling back
to `<capture_dir>/<path>.sdr` / `.frames.jsonl`, `path` being the call's key. The first form
matters when the recorder's RAM spool is on: a held call's files, frames included, sit in the
spool beside its JSON, not yet under the capture folder. The `.dvcf` itself never goes in the
spool: TRP's `settle` moves only its own files (wav, sdr, frames.jsonl, m4a, json) out of it, so
with `outputDir` empty the file is written under the capture folder at the call's key
(`<capture_dir>/<path>.dvcf`, which is beside the call's other files once it is settled).
TRP keeps a call's files until every plugin that takes concluded calls has reported on it, so the
frames file is there when the plugin runs.

**`.sdr`** is MimoSDR's DigitalStream container. TRP writes the frames it kept (the ones its audio
was made from), FEC-encoded again into clean on-air codewords so MimoSDR can play the file, and
appends a tail to each voice record with what the format can't carry: per frame, what TRP's
vocoder made of it (voice, repeat, muted, erasure, tone) and, for IMBE, the Golay errors in u0.
An LDU record carries each IMBE frame's total corrected bits and which slots were lost (`erased`).
TRP's own reader, `trunk_core::trunk::sdr::sdr_frames`, turns the file back into
`Vec<VoiceFrame>`: the information bits after FEC (88 for IMBE, 49 for AMBE), `e0`, `errs`,
`erased`, `kind`. No error correction happens on the way back; the codewords are clean.

**`.frames.jsonl`** is one JSON object per frame: `pos` (seconds), `codec` (`imbe` or `ambe`),
`bits` (the information bits, MSB first, as hex, last byte zero-padded), `e0`, `errs`, `erased`,
`out` (`voice`, `repeat`, `muted`, `erasure`, `tone`).

**The call record.** `call.concluded` carries the whole call JSON as a typed `CallRecord`: every
field TRP writes, including its additions over classic Trunk Recorder (`voice_mode`, `site`,
`clean_voice_pct`, `errorList`, `tones`, `tone_patterns`, `signals`, `transmission_id`,
`start_time_ms`, …), with `srcList` entries carrying each talker's `pos` in seconds into the audio.

**The catch-up source.** `host.calls(CallsQuery)` pages the recorder's call store newest first
(`cursor` / `next`, `from` inclusive, `to` exclusive, up to 500 a page); each entry is
`{ path, record, audio, json }`. The frames file's presence is not reported; the plugin checks
the filesystem.

**What trunk-recorder's callback actually sends.** Checked in trunk-recorder's op25 fork
(`p25p1_fdma.cc`, `p25p2_tdma.cc`, `rx_sync.cc`):

| Codec | `codec_type` | params | What the words are |
|---|---|---|---|
| IMBE (P25 Phase 1) | 0 | 8 | op25's `u[0..7]`: 12, 12, 12, 12, 11, 11, 11 bits, and u7's 7 bits held shifted left one (an 8-bit field, low bit 0) |
| AMBE+2 (P25 Phase 2) | 1 | 4 | the four **information words** u0 (12 bits), u1 (12), u2 (11), u3 (14): 49 bits |
| AMBE+2 (DMR) | 2 | 4 | the same four words |
| AMBE (D-STAR) | 3 | 9 | not produced by TRP |

**This contradicts `DVCF_SPEC.md` §7.2**, which describes the AMBE payload as the 72-bit channel
frame in nine bytes. The C++ plugin writes the words above, straight from the callback, so every
existing AMBE capture holds words, not channel bytes. This plugin writes the words too (parity
with the files that exist) and the spec revision corrects §7.2. Nothing downstream reads AMBE
today: IMBE-ASR filters to `codec_type == 0`.

TRP's `VoiceFrame.bits` are exactly these words, bit-serial MSB first:
`trunk_core::p25::voice::imbe_bits_to_params` gives op25's `u[0..7]` from the 88 IMBE bits
(including u7's shift), and the 49 AMBE bits split 12/12/11/14 into u0..u3.

## The plugin

Id `dvcf`, name "DVCF", a TRP plugin from the template, Rust, **GPL-3.0-or-later** (it links
TRP's `trunk-core`, which is GPL-3; accepted). Repository `trunk-reporter/trunk-plugin-dvcf`
(the C++ one stays `tr-plugin-dvcf`).

Subscribes to `call.concluded` only. Asks for no extra audio formats.

### Dependencies

- `trunk-recorder-plugin` and `trunk-core`, both git dependencies on TrunkRecorder/trunk-recorder-pro
  at the same pinned revision (PR #104's tip at build time), moving together; `default-features
  = false` on the SDK as the template has it. `trunk-core` pulls `sdr-stream`, `rustfft` and the
  cipher crates: slower builds than the transcribe plugin, nothing to do about it. `rust-version`
  follows the workspace (1.99, which `sdr-stream` declares).
- `serde`, `serde_json`, `schemars` as the template has them. Nothing else at runtime.

### Settings

Few, all live (`live_settings`), drawn on the plugin's card:

| Key | Default | Meaning |
|---|---|---|
| `outputDir` | `""` | Where files go. Empty: under the capture folder at the call's key (`<capture_dir>/<path>.dvcf`, which is beside the call's other files once it is settled; the spec's sidecar rule). Otherwise a folder, under which the call's own relative layout is mirrored (`<short_name>/<Y>/<M>/<D>/<base>.dvcf`). |
| `retentionDays` | `0` | Delete `.dvcf` files older than this many days; 0 keeps everything. Only `.dvcf` files the plugin's layout accounts for are touched, nothing else in the capture folder. |
| `catchUp` | `true` | Convert past calls that still have a frames file and no `.dvcf`: at start, then every 10 minutes. |
| `catchUpDays` | `30` | How far back the catch-up looks (0: as far as the call store goes). |

Per system (`system_config`): `enabled` (default true), so capture can be limited to the systems
being studied.

### The file

One `.dvcf` per call, SymbolStream v2 binary framing, wire version 2, in this order:

1. **CALL_START** (0x02): `talkgroup_id` = `record.talkgroup`; `frequency_hz` = `record.freq`;
   `timestamp_us` = `record.start_time_ms × 1000`; `call_id` = `record.call_num` (truncated to
   32 bits); `system_name` = `record.short_name`.
2. **CODEC_FRAME** (0x01) for each kept frame, in order:
   - `talkgroup_id`, `call_id` as above.
   - `src_id`: the talker whose `srcList` entry has the greatest `pos` not after the frame's own
     position (frame k is at k × 20 ms); 0 before the first talker or when the list is empty.
   - `timestamp_us` = start + k × 20 000.
   - `codec_type`: 0 for IMBE; 1 for AMBE when `record.voice_mode` is `p25_phase2`; 2 when it is
     `dmr`; 6 when it is `nxdn48` or `nxdn96`. (A `.sdr` with IMBE frames is Phase 1 whatever
     the mode says; an AMBE `.sdr` under an unexpected mode is written as 1 with a warning.)
   - `param_count`: 8 for IMBE, 4 for AMBE.
   - `errs`: the frame's `errs`, capped at 255; 0 for an erased frame (TRP says its count is
     meaningless then).
   - `flags` bit 0 (silence) set when the frame is `erased` or its `kind` is `repeat`, `muted` or
     `erasure`: its codewords are a reconstruction, not what was heard. A `tone` frame is as
     sent: flag clear.
   - `codec_params`: `imbe_bits_to_params(bits)` for IMBE; the 12/12/11/14 split of the 49 AMBE
     bits for AMBE.
3. **CALL_METADATA** (0x05): one JSON object with the spec's keys filled from the record
   (`tg_tag`, `tg_alpha_tag`, `tg_group`, `tg_description`, `signal`, `noise`, `freq_error`,
   `spike_count`, `emergency`, `priority`, `phase2_tdma`, `tdma_slot`, `patched_tgs`, `src_list`
   with `src`, `time`, `pos`, `emergency`, `signal_system`, `tag`), each omitted when the record
   has nothing for it, **plus** `trunk_recorder_call`: the complete TRP call record as its JSON
   file has it (serialised with `CallRecord::to_json`, so field order and number formats match
   the `.json` on disk), **plus** `trunk_recorder_pro`: `{ "version": <host version>,
   "plugin": "dvcf <plugin version>", "frames_file": "sdr" | "frames.jsonl" }`.
   The talkgroup columns map as the C++ plugin and both recorders' JSON agree (checked in
   `call_concluder.cc` and TRP's `record.rs`): `tg_alpha_tag` = TRP's `talkgroup_tag` (the alpha
   tag), `tg_tag` = `talkgroup_group_tag` (the talkgroup file's Tag column, "Fire Dispatch"),
   `tg_group` = `talkgroup_group` (the Category column), `tg_description` =
   `talkgroup_description`. `signal`, `noise`, `freq_error`, `spike_count`, `emergency`,
   `priority`, `phase2_tdma` and `tdma_slot` are always written (null levels as JSON null), the
   tag strings and `patched_tgs` only when non-empty, as the C++ does.
4. **CALL_END** (0x03): `talkgroup_id`, `call_id`, `src_id` = the last `srcList` talker (0 if
   none), `frequency_hz`, `duration_ms` = `record.call_length_ms`, `error_count` = the sum of the
   frames' `errs` (capped at 2³²−1), `encrypted` = `record.encrypted`, `system_name`.

No HEARTBEAT in a file. Little-endian throughout, as the spec says.

Written to `<dir>/<base>.dvcf.tmp` then renamed over `<base>.dvcf`: whole or absent. An existing
`.dvcf` is overwritten only by the live path (a call concluded again is a rare recorder restart
case, and the newer record wins); the catch-up pass never overwrites.

### The spec revision

Delivered with the plugin as a change to `DVCF_SPEC.md` in tr-plugin-dvcf (a pull request there,
not a copy here): the document goes to **revision 2.1** in its heading; the wire `version` byte
stays 2, since readers are told to ignore unknown JSON keys and nothing else changes. The revision:

- §3.6: documents `trunk_recorder_call` and `trunk_recorder_pro` as optional keys written by this
  plugin, with a one-line description of each and a pointer to TRP's call JSON documentation for
  the record's fields.
- §7.2: corrected to describe the AMBE payload as the four information words (12/12/11/14 bits,
  49 bits in all, low bits significant), as the callback and both plugins write; the
  "channel bytes" text and its reconstruction example removed, with a note that files written
  before this revision also hold words.
- §6: notes that TRP produces types 0, 1, 2 and 6 only.
- §9.1: notes that `flags` bit 0 marks frames the recorder's vocoder did not play as received.

### Live path

On `call.concluded`:

1. Skip, with `call.result skipped` and a reason, when: the system is disabled in the plugin's
   settings; `record.audio_type` is `analog`; `voice_mode` is `provoice`; the call has no audio
   (`call_length_ms` and `call_length` both 0: an encrypted or silent call kept with keepEncrypted /
   keepSilent, for which TRP writes no frames; reason "no audio", no warning); or no frames file exists
   (reason "no frames file: turn on captureFrames in Recording settings"). The missing-file case
   also sets the plugin's status to **warning** with that message, once, until a call with a
   frames file arrives.
2. Otherwise push the call onto the work queue and return. The queue is the plugin's own, not
   the SDK's `CallQueue`: that one answers `call.result` for every item (catch-up jobs must stay
   silent), has no priority between items, and sends metrics of its own that would alternate
   with the plugin's. The transcribe plugin's queue (`queue.rs` in trunk-plugin-transcribe, by
   the same author) already does what's needed, live jobs ahead of catch-up ones on one worker,
   retries with short delays, a clean shutdown, and the builder should start from it rather than
   write a third. One worker; conversion is milliseconds. `call.result` for live calls carries
   `took_ms` (`Host::call_result_took`).
3. The worker: read the frames file, parse it (`sdr_frames` or the JSON lines), build the
   messages, write the file atomically, report `ok` with the output path as the message. A
   frames file that parses to zero frames, or to frames of a codec the mode can't have, is
   `failed` with the reason (the file stays for a look). An unreadable or unwritable file is
   `failed`; the SDK retries it.

### Catch-up pass

With `catchUp` on: at start, then every 10 minutes, page the call store newest first from
`now − catchUpDays` (no `from` when 0), 500 a page, `system` unset. For each entry: skip when the
system is disabled, the record is analog or ProVoice or has no audio, the output `.dvcf` already exists, or no
frames file exists; otherwise queue it as a catch-up job. The pass stops early at the first page
where nothing was queued and nothing lacked only a `.dvcf` (everything older is settled), and
always stops at the end of the store or on a page `error` (store off: warn once). Catch-up jobs
take the same worker as live calls but live calls go first. No `call.result` for catch-up jobs
(they aren't `call.concluded` calls); their count is logged at the end of each pass and shown in
metrics.

### Retention

With `retentionDays` > 0, every hour: walk the output layout (the capture folder's
`<system>/<Y>/<M>/<D>` tree, or `outputDir`'s), delete `.dvcf` files whose modification time is
older, remove nothing else. In the capture folder TRP's own cleanup removes everything it knows;
this only removes what it doesn't.

### Status, metrics, shutdown

- Status: ok normally; warning while calls arrive with no frames file, or the store is off during
  catch-up; error only when the settings are refused, the output folder can't be made (tried again
  with every accepted settings), or the last write failed (until the next one works); then the
  plugin still answers `failed` per call rather than exiting.
- Metrics: queue depth, in flight, written (run total), skipped by reason, failed, catch-up queued
  and written, bytes written.
- Shutdown: finish the live or saved job in hand, write nothing new, exit within the grace period;
  queued catch-up jobs are dropped and one in hand is not waited for (the next pass finds them again). Queued live calls are **saved**,
  not failed: their keys and frames-file paths go to `queue.jsonl` in the plugin's data folder,
  and the next start works them first, before the catch-up pass, dropping with a log line any
  whose frames file is gone. (A `failed` answer keeps the call's files only when the user has
  **Keep when a plugin fails** on; otherwise the frames file would go with the audio and the
  call could never be converted. Saving loses nothing. A saved call is answered with its `call.result` when it is worked
  after the restart: the recorder keeps the pending entry across a plugin-only restart and
  ignores a result for a call it no longer knows.)

### Testing

The template's `testing` module runs the plugin against scripted `hello` / `call.concluded` /
`calls` messages. Fixtures: a small `.sdr` of each mode and a `frames.jsonl`, made with
`trunk-core`'s own writer (`call_sdr`, `frames_jsonl`) from synthetic `VoiceFrame`s in the test,
so the test proves the round trip against TRP's real encoder. A test-only `.dvcf` reader checks
framing, counts, the per-frame fields, the metadata JSON and the trailer. One test runs
`scripts/validate_dvcf_fixture.py` from tr-plugin-dvcf on an output file if Python is present
(skipped otherwise), so the C++ project's own validator passes on this plugin's output.

Live: `trunk-pro plugin run ./target/debug/dvcf <TRP folder> --limit 5` replays recorded calls,
which must have been recorded with `captureFrames` on.

## Left to the builder

- Module layout; the queue's retry count and delays.
- Whether `src_list`'s `time` is `SrcEntry.time` as is (Unix seconds) — yes, as the C++ writes.
- Exact warning wording.
- How the `.sdr` reader is wrapped so a `frames.jsonl`-for-all-modes option in TRP (a possible
  later upstream request) slots in without touching the writer.

## Decided for the user, listed so they can check

- Wire version stays 2; the spec document goes to 2.1.
- AMBE params are the four information words, matching the C++ plugin's real output, not §7.2's
  text; the spec gets corrected.
- `call_id` is `call_num`; timestamps come from `start_time_ms`, not the `.sdr` header.
- The silence flag covers erased, repeated, muted and erasure frames; tone frames are clean.
- The whole call record rides under `trunk_recorder_call`, plus a small `trunk_recorder_pro`
  provenance object.
- Catch-up is on by default, 30 days back, every 10 minutes, never overwriting.
- Retention is hourly and by file modification time.
- The queue is the plugin's own, grown from the transcribe plugin's, because the SDK's answers
  the recorder for every item and can't keep catch-up jobs silent or put live calls first.
- Queued live calls are saved across a restart rather than failed, so a frames file is never
  lost to a shutdown; each is answered with its `call.result` when it is worked after the
  restart (the recorder ignores a result for a call it no longer knows).
- The frames file is looked for beside the call's JSON first (the RAM spool case), then under
  the capture folder.
