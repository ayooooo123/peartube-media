#!/usr/bin/env python3
"""Hash-check the tracker corpus and compare complete renders with openmpt123.

Build first: cargo build --release -j 2 -p codec-tracker --example render
This runner never downloads or builds anything. WAV metadata is ignored.
"""
import argparse
import array
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile


def wave_pcm(path):
    with path.open('rb') as f:
        if f.read(12)[:4] != b'RIFF':
            raise ValueError('not RIFF WAV')
        fmt = None
        while header := f.read(8):
            kind, size = struct.unpack('<4sI', header)
            if kind == b'fmt ':
                fmt = f.read(size)
            elif kind == b'data':
                if fmt is None or struct.unpack_from('<HHI', fmt) != (3, 2, 48000) or struct.unpack_from('<H', fmt, 14)[0] != 32:
                    raise ValueError('oracle must render 48 kHz stereo float32 WAV')
                return f.read(size)
            else:
                f.seek(size, 1)
            if size & 1:
                f.seek(1, 1)
    raise ValueError('missing WAV data')


def compare(actual, expected):
    a, b = array.array('f'), array.array('f')
    a.frombytes(actual)
    b.frombytes(expected)
    if sys.byteorder != 'little':
        a.byteswap()
        b.byteswap()
    n = min(len(a), len(b))
    signal = math.fsum(v * v for v in b)
    noise = math.fsum((x - y) ** 2 for x, y in zip(a, b))
    noise += math.fsum(v * v for v in a[n:]) + math.fsum(v * v for v in b[n:])
    snr = 10 * math.log10(signal / noise) if signal > 0 and noise > 0 else (math.inf if noise == 0 else -math.inf)
    first = next((i // 2 for i, (x, y) in enumerate(zip(a, b)) if x != y), None)
    return dict(frames=len(a) // 2, reference_frames=len(b) // 2,
                snr_db=snr if math.isfinite(snr) else str(snr), exact=actual == expected,
                first_different_frame=first,
                passed=len(a) == len(b) and snr >= 90 and all(math.isfinite(v) for v in a))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--corpus', type=Path, default=Path(os.environ.get('TRACKER_CORPUS', Path.home() / 'projects/oracles')))
    p.add_argument('--renderer', type=Path, required=True)
    p.add_argument('--oracle', default='/opt/homebrew/bin/openmpt123')
    p.add_argument('--report', type=Path, required=True)
    p.add_argument('--filter', default='')
    p.add_argument('--timeout', type=float, default=120)
    args = p.parse_args()
    version = subprocess.run([args.oracle, '--version'], capture_output=True, text=True, check=True)
    oracle_version = version.stdout + version.stderr
    if '0.8.9' not in oracle_version:
        raise SystemExit('requires libopenmpt 0.8.9: ' + oracle_version)
    pins = Path(__file__).resolve().parents[1] / 'corpus/tracker.tsv'
    records = []
    for line in pins.read_text().splitlines():
        if line.startswith('#') or not line:
            continue
        digest, relative, source = line.split('\t')
        if args.filter not in relative:
            continue
        path = pins.parent.parent / relative[5:] if relative.startswith('repo:') else args.corpus / relative
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise SystemExit('hash mismatch: ' + relative)
        record = dict(path=relative, sha256=digest, source=source)
        try:
            with tempfile.TemporaryDirectory(prefix='tracker-oracle-') as d:
                copy = Path(d) / path.name
                shutil.copyfile(path, copy)
                ref = subprocess.run([args.oracle, '--render', '--quiet', str(copy)], capture_output=True, timeout=args.timeout)
                if ref.returncode:
                    raise RuntimeError('oracle: ' + ref.stderr.decode(errors='replace'))
                output = Path(d) / 'actual.f32'
                ours = subprocess.run([str(args.renderer), str(path), str(output)], capture_output=True, timeout=args.timeout)
                if ours.returncode:
                    raise RuntimeError('renderer: ' + ours.stderr.decode(errors='replace'))
                record.update(compare(output.read_bytes(), wave_pcm(Path(str(copy) + '.wav'))))
        except (subprocess.TimeoutExpired, RuntimeError, ValueError) as error:
            record.update(passed=False, error=str(error))
        records.append(record)
        print(json.dumps(record), flush=True)
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(dict(oracle=oracle_version, results=records), indent=2) + '\n')
    passed = sum(r['passed'] for r in records)
    print(f'{passed}/{len(records)} matched at >=90 dB with equal frame counts', flush=True)
    return 0 if passed == len(records) and records else 1


if __name__ == '__main__':
    sys.exit(main())
