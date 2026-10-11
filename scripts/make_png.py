import struct, zlib, sys

# Minimal solid-colour PNG generator (stdlib only) for smoke tests.
def png(path, w=100, h=60, rgb=(200, 30, 30)):
    raw = b""
    row = bytes(rgb) * w
    for _ in range(h):
        raw += b"\x00" + row  # filter byte 0 + RGB pixels
    def chunk(typ, data):
        c = struct.pack(">I", len(data)) + typ + data
        return c + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF)
    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    idat = zlib.compress(raw, 9)
    with open(path, "wb") as f:
        f.write(sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b""))

if __name__ == "__main__":
    png(sys.argv[1] if len(sys.argv) > 1 else "card.png")