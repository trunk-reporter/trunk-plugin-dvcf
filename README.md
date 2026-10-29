# DVCF

A Trunk Recorder Pro plugin that writes one `.dvcf` file for every digital call, from the vocoder frames Trunk Recorder
Pro already keeps. The file is SymbolStream v2, the format the classic `mqtt_dvcf` plugin writes, and holds everything
about the call but the audio: the codec frames, who was talking, and the call's full record.

**Turn on `captureFrames` in Recording settings first. It is off by default.** Without it Trunk Recorder Pro keeps no
frames, so every call is skipped with the reason `no frames file: turn on captureFrames in Recording settings`, and the
plugin's card shows a warning with the same words. The warning goes away when a call with a frames file arrives.

## Which calls get a file

| Call | File |
| --- | --- |
| P25 Phase 1 (IMBE) | yes |
| P25 Phase 2 and DMR (AMBE+2) | yes |
| NXDN | yes |
| Analog | no (skipped: `analog`) |
| ProVoice | no (skipped: `ProVoice`) |

A system can also be left out: per system, `enabled` (default on). Its calls are skipped with the reason `system disabled`.

## Installing

From the Trunk Recorder Pro plugin registry, once the plugin is listed there. Or download a release archive,
`dvcf-<version>-<target>.tar.gz` (`.zip` on Windows), and install it through the recorder's **Install from file**.

## Settings

All four apply while the recorder runs. [`examples/settings.json`](examples/settings.json) is a complete settings object.

| Key | Default | |
| --- | --- | --- |
| `outputDir` | empty | Where the files go. Empty: in the capture folder, next to each call's other files. Otherwise an absolute folder path. |
| `retentionDays` | `0` | Delete `.dvcf` files older than this many days. 0 keeps them for ever. |
| `catchUp` | `true` | Convert past calls that still have a frames file and no `.dvcf`: at start, then every 10 minutes. |
| `catchUpDays` | `30` | How far back the catch-up looks. 0: as far as the recorder's call store goes. |

A relative `outputDir` is refused, and the card says so until good settings come.

## Where the files go

Empty `outputDir`: `<base>.dvcf` goes in the capture folder, next to the call's other files. (A call the recorder holds
in its RAM spool has its other files moved there when it is settled.) With `outputDir` set, the call's own layout is
mirrored under it: `<outputDir>/<system>/<Y>/<M>/<D>/<base>.dvcf`. Pair a `.dvcf` with the call's audio by base name.

A file is written to `<base>.dvcf.tmp` and renamed, so a reader sees it whole or not at all. A call that concludes
again (after a recorder restart, say) has its file replaced by the newer one.

**Retention.** With `retentionDays` above 0, once an hour the plugin walks the output layout and deletes `.dvcf` files
whose modification time is older. It removes nothing else. In the capture folder, Trunk Recorder Pro's own cleanup
removes the call's other files; this removes the `.dvcf` files it doesn't know about.

## Catching up

With `catchUp` on, the plugin asks the recorder's call store for calls from the last `catchUpDays` days, newest first,
and converts each one that still has a frames file and no `.dvcf`. At start it walks the whole window; the passes every 10
minutes stop at the first page with nothing left to do. Live calls always go ahead of catch-up work. The catch-up **never
overwrites** a `.dvcf` that is there. If a pass finds more than 2000 calls to convert, it pauses and the next pass goes on.
The log says how many were queued at the end of each pass. If the call store is off, the plugin warns in the log at each pass that finds it off, and the card shows a warning until a pass ends cleanly.

A call's frames file only lasts as long as Trunk Recorder Pro keeps the call, so catch-up can only reach calls still on disk.

## What the file holds

Per call, in order: CALL_START, one CODEC_FRAME for each frame the recorder kept (in order, bit for bit), CALL_METADATA,
CALL_END. No audio. The format is described in `DVCF_SPEC.md` in
[trunk-reporter/tr-plugin-dvcf](https://github.com/trunk-reporter/tr-plugin-dvcf) (revision 2.1 once its pull request
lands). Each frame carries its talker (`src_id`, from the call's source list), its error count and a silence flag, set for
frames the recorder's vocoder reconstructed (erased, repeated, muted) rather than played as received.

CALL_METADATA has the keys the spec names (talkgroup tags, signal, noise, frequency error, emergency, priority, the source
list, ...) and two this plugin adds:

- `trunk_recorder_call`: Trunk Recorder Pro's complete call record, as its `.json` file holds it.
- `trunk_recorder_pro`: `version` (the recorder's), `plugin` (`dvcf <version>`) and `frames_file` (`sdr` or `frames.jsonl`).

Readers are told to ignore keys they don't know, so existing readers are not affected.

**AMBE codec params.** For AMBE+2 (P25 Phase 2, DMR) the four params are the four information words, of 12, 12, 11 and 14
bits, as the C++ plugin writes them from the decoder's callback. Revision 2.1 of the spec corrects section 7.2 to say so.
IMBE has eight params, as before.

## Restart

When the recorder stops, or the plugin is restarted, live calls still waiting (or one still being converted when the
grace period runs out) are saved to `plugin-data/dvcf/queue.jsonl`. The next start works them first, before the
catch-up, and answers each with its result as it is worked; it logs and drops any whose frames file is gone. Waiting
catch-up work is not saved: the next pass finds it again.

## Status

**Ok** says where the files go. **Warning** while calls arrive with no frames file (see the top), or the call store
answers an error during catch-up. **Error** when the settings are refused or the output folder cannot be made; calls then
fail one by one, and the recorder keeps their files only if **Keep when a plugin fails** is on.

## Building

```
cargo build --release
cargo test
```

Needs Rust 1.99 or later. The build pulls in Trunk Recorder Pro's `trunk-core`, so the first one takes a few minutes.

## License

GPL-3.0-or-later. The plugin links `trunk-core` from Trunk Recorder Pro, which is GPL-3.
