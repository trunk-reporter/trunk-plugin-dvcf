# Changelog

## [0.1.0]

- Writes one `.dvcf` file (SymbolStream v2) for every digital call, from Trunk Recorder Pro's frame capture: P25 Phase 1 and 2, DMR and NXDN.
- Carries the whole Trunk Recorder Pro call record in the file's CALL_METADATA, beside the fields `DVCF_SPEC.md` names.
- Puts the files beside the calls or under an output folder, each written whole or not at all, with an optional retention in days.
- Catches up on past calls that still have a frames file and no `.dvcf`, at start and every 10 minutes, and never overwrites.
- Saves live calls still waiting when the recorder stops, and works them first at the next start.
