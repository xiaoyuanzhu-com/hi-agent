#!/usr/bin/env python3
"""Pack existing brand PNGs into a multi-resolution Windows .ico — no deps.

Windows (Vista+) accepts PNG-compressed icon entries, so an .ico is just a
small directory header followed by the verbatim PNG bytes of each size. We
reuse the SPA's brand PNGs (the same "hi" mark the favicon/app icons use) so
the installer's shortcut/Add-Remove-Programs icon matches everything else,
with zero image tooling on the build host.

Usage: make-ico.py <out.ico> <png> [<png> ...]
"""
import struct
import sys


def png_size(path: str) -> int:
    with open(path, "rb") as f:
        head = f.read(24)
    if head[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG")
    # IHDR width is a big-endian u32 at offset 16 (square icons → width==height).
    return struct.unpack(">I", head[16:20])[0]


def main() -> None:
    if len(sys.argv) < 3:
        sys.exit("usage: make-ico.py <out.ico> <png> [<png> ...]")
    out, pngs = sys.argv[1], sys.argv[2:]

    entries = []  # (size, raw_png_bytes)
    for p in pngs:
        with open(p, "rb") as f:
            data = f.read()
        entries.append((png_size(p), data))
    entries.sort(key=lambda e: e[0])

    count = len(entries)
    header = struct.pack("<HHH", 0, 1, count)  # reserved=0, type=1 (icon), count
    offset = 6 + 16 * count                     # data starts after the directory
    directory, blobs = b"", b""
    for size, data in entries:
        b = size & 0xFF  # 256 would encode as 0; our sizes are all < 256
        directory += struct.pack(
            "<BBBBHHII",
            b, b,            # width, height
            0,               # palette colors (0 = none, true-color)
            0,               # reserved
            1,               # color planes
            32,              # bits per pixel
            len(data),       # bytes of image data
            offset,          # offset to image data
        )
        blobs += data
        offset += len(data)

    with open(out, "wb") as f:
        f.write(header + directory + blobs)
    print(f"wrote {out}: {count} sizes {[s for s, _ in entries]}")


if __name__ == "__main__":
    main()
