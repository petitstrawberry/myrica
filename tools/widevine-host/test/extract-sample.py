#!/usr/bin/env python3
# Extract one encrypted sample (bytes, IV and subsample map) from a fragmented
# MP4 CENC segment pair, so the CDM host can be asked to decrypt it.
#
# usage: extract-sample.py --init init.mp4 --media 0001.m4s [--sample N] --out-dir DIR
#
# The same box layout is what scarlet-codecs has to parse on the Rust side, so
# this doubles as the reference for that work.

import argparse
import json
import struct
from pathlib import Path


def iter_boxes(data, start, end):
    position = start
    while position + 8 <= end:
        size = struct.unpack_from(">I", data, position)[0]
        box_type = data[position + 4 : position + 8]
        header = 8
        if size == 1:
            size = struct.unpack_from(">Q", data, position + 8)[0]
            header = 16
        elif size == 0:
            size = end - position
        if size < header or position + size > end:
            break
        yield box_type, position + header, position + size, position
        position += size


def find_box(data, box_type, start=0, end=None):
    if end is None:
        end = len(data)
    for found_type, payload_start, box_end, box_start in iter_boxes(data, start, end):
        if found_type == box_type:
            return payload_start, box_end, box_start
    return None


CONTAINERS = {
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"mvex", b"moof",
    b"traf", b"encv", b"enca", b"sinf", b"schi", b"wave",
}


def deep_find(data, box_type, start=0, end=None):
    if end is None:
        end = len(data)
    for found_type, payload_start, box_end, box_start in iter_boxes(data, start, end):
        if found_type == box_type:
            return payload_start, box_end, box_start
        if found_type in CONTAINERS:
            child_start = payload_start
            if found_type == b"stsd":
                # version, flags, entry count.
                child_start += 8
            elif found_type == b"encv":
                # SampleEntry header (8) plus VisualSampleEntry fields (70).
                child_start += 78
            elif found_type == b"enca":
                # SampleEntry header (8) plus AudioSampleEntry fields (20).
                child_start += 28
            result = deep_find(data, box_type, child_start, box_end)
            if result is not None:
                return result
    return None


def parse_tenc(payload):
    # ISO/IEC 23001-7 tenc. The 2016 edition always carries the pattern
    # nibbles, so the default encryption fields can sit either right after the
    # reserved byte (older writers) or one byte later (Shaka Packager and other
    # current writers). Decide from the box size.
    version = payload[0]
    has_pattern = len(payload) - 4 >= 20
    crypt = skip = 0
    if has_pattern:
        position = 4
        position += 1
        blocks = payload[position]
        position += 1
        crypt = blocks >> 4
        skip = blocks & 0x0F
        is_protected = payload[position]
        position += 1
        iv_size = payload[position]
        position += 1
    else:
        position = 4
        position += 1
        is_protected = payload[position]
        position += 1
        iv_size = payload[position]
        position += 1
    key_id = payload[position : position + 16]
    return {
        "version": version,
        "has_pattern": has_pattern,
        "is_protected": is_protected,
        "iv_size": iv_size,
        "crypt_byte_block": crypt,
        "skip_byte_block": skip,
        "key_id": key_id.hex(),
    }


def parse_senc(payload, default_iv_size):
    flags = int.from_bytes(payload[1:4], "big")
    position = 4
    iv_size = default_iv_size
    if flags & 0x01:
        iv_size = payload[position]
        position += 1
    sample_count = int.from_bytes(payload[position : position + 4], "big")
    position += 4
    entries = []
    for _ in range(sample_count):
        iv = payload[position : position + iv_size]
        position += iv_size
        subsamples = []
        if flags & 0x02:
            count = int.from_bytes(payload[position : position + 2], "big")
            position += 2
            for _ in range(count):
                clear = int.from_bytes(payload[position : position + 2], "big")
                position += 2
                cipher = int.from_bytes(payload[position : position + 4], "big")
                position += 4
                subsamples.append((clear, cipher))
        entries.append((iv, subsamples))
    return entries


def parse_trun(payload):
    flags = int.from_bytes(payload[1:4], "big")
    position = 4
    sample_count = int.from_bytes(payload[position : position + 4], "big")
    position += 4
    data_offset = None
    if flags & 0x01:
        data_offset = int.from_bytes(payload[position : position + 4], "big", signed=True)
        position += 4
    if flags & 0x04:
        position += 4
    samples = []
    for _ in range(sample_count):
        duration = size = sample_flags = cto = None
        if flags & 0x100:
            duration = int.from_bytes(payload[position : position + 4], "big")
            position += 4
        if flags & 0x200:
            size = int.from_bytes(payload[position : position + 4], "big")
            position += 4
        if flags & 0x400:
            sample_flags = int.from_bytes(payload[position : position + 4], "big")
            position += 4
        if flags & 0x800:
            cto = int.from_bytes(payload[position : position + 4], "big", signed=True)
            position += 4
        samples.append(
            {"duration": duration, "size": size, "flags": sample_flags, "cto": cto}
        )
    return {"sample_count": sample_count, "data_offset": data_offset, "samples": samples}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--init", required=True, type=Path)
    parser.add_argument("--media", required=True, type=Path)
    parser.add_argument("--sample", type=int, default=0)
    parser.add_argument("--out-dir", required=True, type=Path)
    args = parser.parse_args()

    init = args.init.read_bytes()
    media = args.media.read_bytes()

    tenc_box = deep_find(init, b"tenc")
    if tenc_box is None:
        raise SystemExit("no tenc box in the initialization segment")
    tenc = parse_tenc(init[tenc_box[0] : tenc_box[1]])

    moof = find_box(media, b"moof")
    if moof is None:
        raise SystemExit("no moof box in the media segment")
    traf = find_box(media, b"traf", moof[0], moof[1])
    if traf is None:
        raise SystemExit("no traf box")
    senc_box = find_box(media, b"senc", traf[0], traf[1])
    trun_box = find_box(media, b"trun", traf[0], traf[1])
    mdat = find_box(media, b"mdat")
    if senc_box is None or trun_box is None or mdat is None:
        raise SystemExit("missing senc, trun or mdat")

    senc = parse_senc(media[senc_box[0] : senc_box[1]], tenc["iv_size"])
    trun = parse_trun(media[trun_box[0] : trun_box[1]])

    index = args.sample
    if index >= len(senc) or index >= len(trun["samples"]):
        raise SystemExit("sample index out of range")

    data_start = None
    if trun["data_offset"] is not None:
        data_start = moof[2] + trun["data_offset"]
    else:
        data_start = mdat[0]
    offset = data_start
    for previous in trun["samples"][:index]:
        offset += previous["size"] or 0
    size = trun["samples"][index]["size"]
    iv, subsamples = senc[index]
    sample = media[offset : offset + size]

    args.out_dir.mkdir(parents=True, exist_ok=True)
    (args.out_dir / "sample.bin").write_bytes(sample)
    info = {
        "key_id": tenc["key_id"],
        "iv": iv.hex(),
        "iv_size": tenc["iv_size"],
        "subsamples": subsamples,
        "size": size,
        "offset": offset,
        "sample_count": trun["sample_count"],
    }
    (args.out_dir / "sample.json").write_text(json.dumps(info, indent=2) + chr(10))

    print("tenc:", json.dumps(tenc))
    print("trun:", json.dumps({"sample_count": trun["sample_count"], "data_offset": trun["data_offset"]}))
    print("sample size:", size, "offset:", offset)
    print("iv:", iv.hex())
    print("subsamples:", subsamples[:4], "count:", len(subsamples))
    print("first bytes:", sample[:8].hex())
    clear = sum(entry[0] for entry in subsamples)
    if clear >= 4 and len(sample) >= 8:
        length = int.from_bytes(sample[0:4], "big")
        print("first clear NAL length:", length, "plausible:", 0 < length <= size)


if __name__ == "__main__":
    main()
