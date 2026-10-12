#!/usr/bin/env python3
"""Writes the six RGBA subtitle images spumux encodes (see generate.sh).

Four colours each: transparent, opaque near-white, opaque near-black and
half-transparent red, arranged as glyph-like strokes in a bordered box.
"""
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
WHITE, BLACK, GREY, CLEAR = (235, 235, 235, 255), (16, 16, 16, 255), (200, 40, 40, 128), (0, 0, 0, 0)


def png(path, width, height, pixel):
    raw = bytearray()
    for y in range(height):
        raw.append(0)
        for x in range(width):
            raw.extend(pixel(x, y))

    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)

    header = struct.pack('>IIBBBBB', width, height, 8, 6, 0, 0, 0)
    with open(path, 'wb') as out:
        out.write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', header) + chunk(b'IDAT', zlib.compress(bytes(raw), 9)) + chunk(b'IEND', b''))


def caption(x0, y0, x1, y1, seed):
    def pixel(x, y):
        if not (x0 <= x < x1 and y0 <= y < y1):
            return CLEAR
        if x < x0 + 2 or x >= x1 - 2 or y < y0 + 2 or y >= y1 - 2:
            return BLACK
        u, v = x - x0, y - y0
        if (u // (3 + seed) + v // 4) % 5 == 0 or (v % 9 in (3, 4) and (u // 7) % 3 != 1):
            return WHITE
        if v % 13 == 6:
            return GREY
        return CLEAR
    return pixel


for name, (w, h) in {'svcd': (480, 480), 'cvd': (352, 480)}.items():
    scale = w / 480
    # The first caption is tall enough to need several 2324-byte sectors.
    for index, (x0, y0, x1, y1) in enumerate([(60, 330, 420, 431), (100, 300, 380, 333), (30, 40, 200, 79)]):
        png(os.path.join(HERE, f'{name}-{index + 1}.png'), w, h, caption(int(x0 * scale), y0, int(x1 * scale), y1, index))
