#!/usr/bin/env python3
"""Extract the first audio track of an MP4/M4A as libxaac test-bench input.

Writes `<out>.raw` (AudioSpecificConfig followed by every sample, in file
order, with no edit-list trimming) and `<out>.meta` (the `-mp4:1` metadata
file read by libxaac's xaacdec: config size, sample sizes, edit list off).
Only reads the container; sample bytes are copied unchanged.

usage: mp4_es.py <input.mp4> <out-prefix>
"""
import struct
import sys


def boxes(data, start, end):
    pos = start
    while pos + 8 <= end:
        size, kind = struct.unpack(">I4s", data[pos:pos + 8])
        header = 8
        if size == 1:
            size = struct.unpack(">Q", data[pos + 8:pos + 16])[0]
            header = 16
        elif size == 0:
            size = end - pos
        if size < header or pos + size > end:
            raise ValueError(f"bad box {kind!r} at {pos}")
        yield kind.decode("latin-1"), pos + header, pos + size
        pos += size


def child(data, start, end, path):
    for name in path:
        for kind, s, e in boxes(data, start, end):
            if kind == name:
                start, end = s, e
                break
        else:
            return None
    return start, end


def descriptor(data, pos):
    tag = data[pos]
    pos += 1
    length = 0
    for _ in range(4):
        byte = data[pos]
        pos += 1
        length = length << 7 | byte & 0x7F
        if not byte & 0x80:
            break
    return tag, pos, pos + length


def audio_specific_config(data, start, end):
    # esds: full box (version/flags), then ES_Descriptor.
    tag, pos, stop = descriptor(data, start + 4)
    assert tag == 3, "ES_Descriptor"
    flags = data[pos + 2]
    pos += 3
    if flags & 0x80:
        pos += 2
    if flags & 0x40:
        pos += 1 + data[pos]
    if flags & 0x20:
        pos += 2
    tag, pos, stop = descriptor(data, pos)
    assert tag == 4, "DecoderConfigDescriptor"
    tag, pos, stop = descriptor(data, pos + 13)
    assert tag == 5, "DecoderSpecificInfo"
    return data[pos:stop]


def main():
    source, prefix = sys.argv[1], sys.argv[2]
    data = open(source, "rb").read()
    moov = child(data, 0, len(data), ["moov"])
    for kind, s, e in boxes(data, *moov):
        if kind != "trak":
            continue
        hdlr = child(data, s, e, ["mdia", "hdlr"])
        if data[hdlr[0] + 8:hdlr[0] + 12] != b"soun":
            continue
        stbl = child(data, s, e, ["mdia", "minf", "stbl"])
        stsd = child(data, *stbl, ["stsd"])
        entry = next(boxes(data, stsd[0] + 8, stsd[1]))
        # AudioSampleEntry: 28 bytes of fields before its child boxes.
        esds = child(data, entry[1] + 28, entry[2], ["esds"])
        asc = audio_specific_config(data, *esds)
        stsz = child(data, *stbl, ["stsz"])
        fixed, count = struct.unpack(">II", data[stsz[0] + 4:stsz[0] + 12])
        sizes = [fixed] * count if fixed else list(
            struct.unpack(f">{count}I", data[stsz[0] + 12:stsz[0] + 12 + 4 * count]))
        stco = child(data, *stbl, ["stco"])
        if stco:
            n = struct.unpack(">I", data[stco[0] + 4:stco[0] + 8])[0]
            chunks = struct.unpack(f">{n}I", data[stco[0] + 8:stco[0] + 8 + 4 * n])
        else:
            co64 = child(data, *stbl, ["co64"])
            n = struct.unpack(">I", data[co64[0] + 4:co64[0] + 8])[0]
            chunks = struct.unpack(f">{n}Q", data[co64[0] + 8:co64[0] + 8 + 8 * n])
        stsc = child(data, *stbl, ["stsc"])
        n = struct.unpack(">I", data[stsc[0] + 4:stsc[0] + 8])[0]
        runs = [struct.unpack(">III", data[stsc[0] + 8 + 12 * i:stsc[0] + 20 + 12 * i]) for i in range(n)]
        samples = []
        index = 0
        for c, offset in enumerate(chunks, start=1):
            per_chunk = [r for r in runs if r[0] <= c][-1][1]
            for _ in range(per_chunk):
                samples.append(data[offset:offset + sizes[index]])
                offset += sizes[index]
                index += 1
        assert index == count, (index, count)
        mdhd = child(data, s, e, ["mdia", "mdhd"])
        version = data[mdhd[0]]
        scale = struct.unpack(">I", data[mdhd[0] + (20 if version else 12):][:4])[0]
        mvhd = child(data, *moov, ["mvhd"])
        mversion = data[mvhd[0]]
        movie_scale = struct.unpack(">I", data[mvhd[0] + (20 if mversion else 12):][:4])[0]
        with open(prefix + ".raw", "wb") as out:
            out.write(asc)
            for sample in samples:
                out.write(sample)
        with open(prefix + ".meta", "w") as meta:
            meta.write(f"-dec_info_init:{len(asc)}\n-g_track_count:1\n")
            meta.write(f"-movie_time_scale:{movie_scale}\n-media_time_scale:{scale}\n")
            meta.write(f"-ia_mp4_stsz_entries:{count}\n")
            for size in sizes:
                meta.write(f"-ia_mp4_stsz_size:{size}\n")
            meta.write("-playTimeInSamples:0\n-startOffsetInSamples:0\n-useEditlist:0\n")
        print(f"{source}: ASC {asc.hex()} ({len(asc)} bytes), {count} samples, {sum(sizes)} bytes")
        return
    raise SystemExit("no audio track")


if __name__ == "__main__":
    main()
