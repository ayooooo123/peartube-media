#!/usr/bin/env python3
"""Separate stock-oracle nondeterminism from seed-controlled native playback proof.

All twelve hash-pinned IT files stay byte-for-byte unchanged. The native helper
uses version-pinned protected libopenmpt state, not a public seed control. Its
result is source-level event/mixer evidence, not parity with stock random seeds.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('tracker_oracle', Path(__file__).with_name('check-tracker.py'))
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
CASES = frozenset(('GlobalVolume-Macro.it', 'RandomPan.it', 'RandomWaveform.it',
                  'gxsmp.it', 'gxsmp2.it', 'swing1.it', 'swing3.it', 'swing4.it',
                  'swing5.it', 'tremolo.it', 'vibrato-oldfx.it', 'vibrato.it'))


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--corpus', type=Path, default=Path(os.environ.get('TRACKER_CORPUS', Path.home() / 'projects/oracles')))
    p.add_argument('--renderer', type=Path, required=True, help='codec-tracker render_seeded example')
    p.add_argument('--seeded-oracle', type=Path, required=True, help='tracker-seeded-oracle.cpp executable')
    p.add_argument('--oracle', default='/opt/homebrew/bin/openmpt123')
    p.add_argument('--report', type=Path, required=True)
    p.add_argument('--seeds', nargs='+', type=lambda s: int(s, 0), default=[0, 0x12345678, 0xffffffff])
    p.add_argument('--stock-repeats', type=int, default=4)
    p.add_argument('--timeout', type=float, default=120)
    args = p.parse_args()
    if args.stock_repeats < 2 or any(not 0 <= s <= 0xffffffff for s in args.seeds):
        p.error('at least two stock repeats and u32 seeds are required')

    def run(cmd):
        result = subprocess.run([str(x) for x in cmd], capture_output=True, timeout=args.timeout)
        if result.returncode:
            raise RuntimeError(result.stderr.decode(errors='replace'))
        return result.stdout.decode(errors='replace') + result.stderr.decode(errors='replace')

    version = run([args.oracle, '--version'])
    if '0.8.9' not in version:
        p.error('requires libopenmpt 0.8.9')
    pins = Path(__file__).resolve().parents[1] / 'corpus/tracker.tsv'
    report = dict(oracle=version, reference_kind='version-pinned protected native PRNG; NOT stock CLI seed parity',
                  source_controls='unchanged; input SHA-256 verified against corpus/tracker.tsv', results=[])
    for line in pins.read_text().splitlines():
        if not line or line.startswith('#'):
            continue
        digest, relative, source = line.split('\t')
        if Path(relative).name not in CASES:
            continue
        path = args.corpus / relative
        if sha(path.read_bytes()) != digest:
            raise SystemExit('hash mismatch: ' + relative)
        record = dict(path=relative, sha256=digest, source=source)
        try:
            with tempfile.TemporaryDirectory(prefix='tracker-random-') as tmp:
                tmp = Path(tmp)
                stock = []
                first = None
                for repeat in range(args.stock_repeats):
                    copy = tmp / f'stock-{repeat}.it'
                    shutil.copyfile(path, copy)
                    run([args.oracle, '--render', '--quiet', copy])
                    pcm = oracle.wave_pcm(Path(str(copy) + '.wav'))
                    if first is None:
                        first = pcm
                    stock.append(dict(run=repeat, pcm_sha256=sha(pcm), compared_to_first=oracle.compare(pcm, first)))
                record['stock'] = stock
                record['stock_distinct_outputs'] = len({r['pcm_sha256'] for r in stock})
                seeded = []
                native_hashes = []
                for seed in args.seeds:
                    ref_path, actual_path = tmp / 'reference.f32', tmp / 'actual.f32'
                    command = [args.seeded_oracle, path, ref_path, seed]
                    log = run(command)
                    reference = ref_path.read_bytes()
                    run(command)
                    repeat_exact = ref_path.read_bytes() == reference
                    run([args.renderer, path, actual_path, seed])
                    actual = actual_path.read_bytes()
                    frames = len(reference) // 8
                    seek = min(12345, max(0, frames - 1024))
                    run([args.renderer, path, actual_path, seed, seek])
                    entry = dict(seed=seed, native_log=log, native_pcm_sha256=sha(reference),
                                 actual_pcm_sha256=sha(actual), native_repeat_exact=repeat_exact,
                                 comparison=oracle.compare(actual, reference), seek_frame=seek,
                                 seek_comparison=oracle.compare(actual_path.read_bytes(), reference[seek * 8:]))
                    entry['passed'] = repeat_exact and entry['comparison']['passed'] and entry['seek_comparison']['passed']
                    native_hashes.append(sha(reference))
                    seeded.append(entry)
                record['seeded'] = seeded
                record['distinct_native_seed_outputs'] = len(set(native_hashes))
                record['passed'] = (record['stock_distinct_outputs'] > 1 and all(s['passed'] for s in seeded))
        except (subprocess.TimeoutExpired, RuntimeError, ValueError) as error:
            record.update(passed=False, error=str(error))
        report['results'].append(record)
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(record), flush=True)
    passed = sum(r['passed'] for r in report['results'])
    print(f'{passed}/{len(CASES)} cases: stock repeat differences and controlled native playback/seek proof', flush=True)
    return 0 if passed == len(CASES) and len(report['results']) == len(CASES) else 1


if __name__ == '__main__':
    sys.exit(main())
