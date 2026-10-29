# From trunk-reporter/tr-plugin-dvcf, unchanged: the C++ plugin's own check of a .dvcf file.
#!/usr/bin/env python3
"""Validate core DVCF SSSP message ordering for a fixture file."""

from __future__ import annotations

import struct
import sys
from pathlib import Path

MAGIC = b"SY"
VERSION = 2
CODEC_FRAME = 0x01
CALL_START = 0x02
CALL_END = 0x03
CALL_METADATA = 0x05


def read_messages(path: Path) -> list[int]:
    data = path.read_bytes()
    pos = 0
    messages: list[int] = []
    while pos < len(data):
        if pos + 8 > len(data):
            raise ValueError("truncated SSSP header")
        magic, version, msg_type, payload_len = struct.unpack_from("<2sBBI", data, pos)
        pos += 8
        if magic != MAGIC or version != VERSION:
            raise ValueError("invalid SSSP header")
        if pos + payload_len > len(data):
            raise ValueError("truncated SSSP payload")
        pos += payload_len
        messages.append(msg_type)
    return messages


def validate(messages: list[int]) -> None:
    if not messages or messages[0] != CALL_START:
        raise ValueError("first message must be CALL_START")
    if messages[-1] != CALL_END:
        raise ValueError("last message must be CALL_END")
    if CODEC_FRAME not in messages:
        raise ValueError("fixture must include at least one CODEC_FRAME")
    if CALL_METADATA in messages and messages.index(CALL_METADATA) > messages.index(CALL_END):
        raise ValueError("CALL_METADATA must appear before CALL_END")


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: validate_dvcf_fixture.py FILE.dvcf", file=sys.stderr)
        return 2
    messages = read_messages(Path(sys.argv[1]))
    validate(messages)
    print("ok: %s messages" % len(messages))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
