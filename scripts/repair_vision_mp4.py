#!/usr/bin/env python3
"""One-off repair for fragmented-MP4 vision minute files missing their `moov`.

Background: the camera-stream persistence used to cache only the first WS chunk
as the "init segment". For fragmented MP4, `MediaRecorder` splits the init
(`ftyp`+`moov`) across the first two chunks, so only the *first* minute file of
each session kept its `moov`; every later file is `ftyp`+`moof`+... with no
`moov` and won't decode. The streaming code is now fixed (init is delimited by
container structure), but files already on disk stay corrupt.

This grafts the `moov` from each session's first (valid) file into the later
broken files of the same session. Files are processed in wall-clock order; the
most recently seen `moov` is the current session's decoder config, so a camera
restart that emits a fresh `moov` naturally takes over from that point.

Usage:
    python3 scripts/repair_vision_mp4.py [ROOT] [--apply]

ROOT defaults to data/memory/raw. Without --apply it's a dry run (reports only).
With --apply, each repaired file is rewritten in place and the original is kept
alongside as <name>.broken (never overwritten if already present).
"""
import struct
import sys
from pathlib import Path


def iter_boxes(data: bytes):
    """Yield (type, start, size) for each top-level MP4 box in `data`."""
    i = 0
    n = len(data)
    while i + 8 <= n:
        size = struct.unpack(">I", data[i : i + 4])[0]
        typ = data[i + 4 : i + 8]
        if size == 1:
            if i + 16 > n:
                break
            size = struct.unpack(">Q", data[i + 8 : i + 16])[0]
        elif size == 0:
            size = n - i
        if size < 8:
            break
        yield typ, i, size
        i += size


def split_init(data: bytes):
    """Return (ftyp_end, moov_bytes_or_None, first_moof_start_or_None)."""
    ftyp_end = None
    moov = None
    moof_start = None
    for typ, start, size in iter_boxes(data):
        if typ == b"ftyp":
            ftyp_end = start + size
        elif typ == b"moov":
            moov = data[start : start + size]
        elif typ == b"moof":
            moof_start = start
            break
    return ftyp_end, moov, moof_start


def collect_files(root: Path):
    """All vision .mp4 files under root, sorted chronologically.

    Path shape: <scene>/vision/<YYYY-MM-DD>/<HH>/<MM>.mp4 — sort per scene by
    (date, hour, minute) so each scene's session order is honoured.
    """
    files = []
    for p in root.glob("*/vision/*/*/*.mp4"):
        parts = p.parts
        # .../<scene>/vision/<date>/<HH>/<MM>.mp4
        scene, date, hh, mm = parts[-5], parts[-3], parts[-2], p.stem
        files.append((scene, date, hh, mm, p))
    files.sort(key=lambda t: (t[0], t[1], t[2], t[3]))
    return files


def main():
    args = [a for a in sys.argv[1:] if a != "--apply"]
    apply = "--apply" in sys.argv[1:]
    root = Path(args[0]) if args else Path("data/memory/raw")
    if not root.exists():
        print(f"root not found: {root}", file=sys.stderr)
        return 1

    files = collect_files(root)
    if not files:
        print(f"no vision .mp4 files under {root}")
        return 0

    current_moov = None
    current_scene = None
    valid = repaired = skipped = unrepairable = 0

    for scene, _date, _hh, _mm, path in files:
        if scene != current_scene:
            current_scene = scene
            current_moov = None  # don't carry a moov across scenes
        data = path.read_bytes()
        ftyp_end, moov, moof_start = split_init(data)

        if moov is not None:
            current_moov = moov
            valid += 1
            continue

        # Missing moov → needs grafting.
        if moof_start is None or ftyp_end is None:
            print(f"SKIP (no ftyp/moof boundary): {path}")
            unrepairable += 1
            continue
        if current_moov is None:
            print(f"UNREPAIRABLE (no session moov seen yet): {path}")
            unrepairable += 1
            continue

        repaired_bytes = data[:ftyp_end] + current_moov + data[ftyp_end:]
        rel = path.relative_to(root)
        if apply:
            backup = path.with_suffix(path.suffix + ".broken")
            if not backup.exists():
                path.rename(backup)
            else:
                # backup already there from a prior run; read original from it
                data = backup.read_bytes()
                ftyp_end2, _, _ = split_init(data)
                repaired_bytes = data[:ftyp_end2] + current_moov + data[ftyp_end2:]
            path.write_bytes(repaired_bytes)
            print(f"REPAIRED {rel} (+{len(current_moov)} bytes moov)")
        else:
            print(f"would repair {rel} (+{len(current_moov)} bytes moov)")
        repaired += 1

    print(
        f"\n{valid} already valid, {repaired} "
        f"{'repaired' if apply else 'to repair'}, "
        f"{skipped} skipped, {unrepairable} unrepairable"
    )
    if not apply and repaired:
        print("dry run — re-run with --apply to write changes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
