#!/usr/bin/env python3
"""Usage: prepare.py <ISO_USAC-directory> <xaacdec>
Fetch the exact public ISO archive members (CRC-checked), copy their ASC/AUs
without edits, and run unmodified libxaac for full raw PCM. A separate M3
derivative flips only independent-window TNS order; its native PCM must equal
the original. Nothing rewrites the canonical inputs or ISO reference WAVs.
"""
import json
import os
from pathlib import Path
import subprocess
import sys

here = Path(__file__).resolve().parent
root = Path(sys.argv[1]).resolve()
decoder = str(Path(sys.argv[2]).resolve())
root.mkdir(parents=True, exist_ok=True)
os.chdir(root)
sys.path.insert(0, str(here))
from remote_zip import fetch
catalog = here / 'catalog.json'
members = json.loads(catalog.read_text())['members']
fetch(catalog, list(members))
output = root / 'libxaac-out'
output.mkdir(exist_ok=True)

def decode(prefix):
    command = [decoder, f'-ifile:{prefix}.raw', f'-imeta:{prefix}.meta', f'-ofile:{prefix}.wav',
               '-mp4:1', '-pcmsz:24', '-dmix:0', '-tostereo:0', '-peak_limiter_off:1', '-err_conceal:0']
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f'{command}: {result.stdout}\n{result.stderr}')
    print(' '.join(command))

for member in members:
    if not member.startswith('compressedMp4/'):
        continue
    source = root / 'members' / member
    prefix = output / source.stem
    subprocess.run([sys.executable, str(here.parent / 'libxaac/mp4_es.py'), str(source), str(prefix)], check=True)
    decode(prefix)

# This MP4 has one CPE, no extension; the first six bits are independence,
# core_mode[0], core_mode[1], tns_active, common_window, then tns_on_lr.
original = output / 'Fd_2_c1_WinTns_0x0c'
raw = bytearray(original.with_suffix('.raw').read_bytes())
meta = original.with_suffix('.meta').read_text()
position = int(next(l.split(':')[1] for l in meta.splitlines() if l.startswith('-dec_info_init:')))
sizes = [int(l.split(':')[1]) for l in meta.splitlines() if l.startswith('-ia_mp4_stsz_size:')]
flips = []
for i, size in enumerate(sizes):
    assert raw[position] & 0x60 == 0, 'FD pair'
    if raw[position] & 0x18 == 0x10:
        assert raw[position] & 4
        raw[position] ^= 4
        flips.append(i)
    position += size
assert position == len(raw) and len(flips) == 108
prefix = output / 'independent-tns'
prefix.with_suffix('.raw').write_bytes(raw)
prefix.with_suffix('.meta').write_text(meta)
decode(prefix)
assert prefix.with_suffix('.wav').read_bytes() == original.with_suffix('.wav').read_bytes(), 'native TNS PCM differs'
print('Independent-window TNS: native PCM identical; changed AUs', flips)
