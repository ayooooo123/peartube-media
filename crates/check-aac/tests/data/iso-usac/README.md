# Independent USAC FD conformance

Source: [ISO/IEC 23003-7 publicly available conformance sequences](https://standards.iso.org/iso-iec/23003/-7/ed-1/en/ISO_IEC23003-7_Conformance_Sequences.zip). `catalog.json` records exact ZIP member offsets, compressed/physical lengths and published CRC-32 values; `SHA256SUMS` pins the downloaded MP4s, ISO reference WAVs and full raw libxaac WAVs. `manifest.tsv` adds MD5s for the Rust test, actual access-unit counts, MP4 edit-list boundaries, and per-channel SNR floors.

These ISO assets are **external prerequisites**, not redistributed under this repository's MIT license. No ISO implementation source is shipped. The test fails with setup instructions when data is absent; it never skips a missing vector. The Python extraction/fetch tools are project-authored and MIT. Native decoder source/build/license details are in [../libxaac/README.md](../libxaac/README.md).

## Reproduce

With unmodified libxaac `2fbadd57bca46693b7a077bcf1513131106d960c` built as described there, from the check-aac crate:

```sh
python3 tests/data/iso-usac/prepare.py \
  "$HOME/projects/oracles/iso-usac" \
  "$HOME/projects/oracles/libxaac-build-macos-c/xaacdec"
ISO_USAC="$HOME/projects/oracles/iso-usac" \
  CARGO_TARGET_DIR="$HOME/projects/peartube-media-wt/.targets/t3" \
  cargo test -j 2 -p check-aac --test iso_reference -- --nocapture
```

`ISO_USAC` defaults to `$HOME/projects/oracles/iso-usac`. Preparation fetches only the pinned members via HTTP ranges (not the entire 3.5 GB ZIP), verifies original CRCs, extracts untouched ASC and every AU, and prints the exact native decode command. Native flags: `-mp4:1 -pcmsz:24 -dmix:0 -tostereo:0 -peak_limiter_off:1 -err_conceal:0`, without loudness normalization or edit-list trimming.

The canonical conformance comparisons always use the original MP4s without any rewriting. ISO WAVs contain the presentation interval; native WAVs contain every raw 1024-sample AU. Both are compared separately, every sample, every channel, without fitted gain or searched alignment:

- 7350 Hz (`0x0c`): 218 AUs = 223232 raw frames/channel; elst skip 2225, presentation 220501, trailing 506.
- 44100 Hz (`0x04`): 1295 AUs = 1326080 raw frames/channel; elst skip 2220, presentation 1323001, trailing 859.

The 16 vectors cover Cp/SfbCp/TnsCp, window-switched complex prediction, TNS, M/S, noise filling and window transitions. Counters assert actual nonzero imaginary prediction coefficients, previous-frame MDST, and complex short windows; an encoder option or a filename alone is not feature evidence. Existing FATE FFmpeg floors remain unchanged.

Final per-channel measurements span 108.525324–123.220004 dB against ISO
presentation PCM and 98.432450–108.392619 dB against complete native PCM.
Every entry in `manifest.tsv` pins its own measured value minus 0.5 dB
(both channels, both references); the 90 dB minimum is not substituted
for these stronger floors.

## Independent-window TNS (M3)

`prepare.py` also creates a **separate derivative** of `Fd_2_c1_WinTns_0x0c`, leaving the MP4, its ISO reference, and the original elementary stream intact. Only bit 5 (zero-based) of 108 AUs changes: `tns_on_lr=1` becomes 0, and only where `tns_active=1, common_window=0`. Both core modes are asserted FD. No spectra, TNS coefficients, configuration, or presentation boundaries change.

Unmodified native libxaac decodes both forms to byte-identical complete 24-bit WAVs. The derivative exercises 130 active independent-window TNS channels. The pre-fix Rust executable measured 10.087809 / 9.517492 dB over the full raw derivative; the corrected decoder measures 106.112463 / 104.098594 dB, with floors 105.612463 / 103.598594 dB. The permanent regression requires the fork's original/derivative PCM to be exactly equal and verifies both native WAV hashes. This is a new equivalence fixture, not an altered canonical input used to evade a failing fidelity assertion.

The original native encoder was also exercised (`xaacenc`, same source revision, `-aot:42 -usac:1 -ccfl_idx:1 -esbr:0 -cmpx_pred:1 -tns:1 -br:96000`). It synchronizes stereo block switching (`iusace_enc_main.c:1118-1120`); the independent-transient signal still emitted zero independent-window TNS channels. Complex-tone searches did not supply the needed imaginary-prediction case. The ISO encoded corpus resolves both evidence requirements instead.

## Algorithm authority and baseline failures

[ISO/IEC 23003-3 reference software, 2019](https://standards.iso.org/iso-iec/23003/-3/ed-2/en/ISO_IEC_23003-3_2019(E)_Reference_Software.zip), archive SHA-256 `e7752838a86c9e525fc6941889ebbc083d9fc1ae19fbbc52971ef5eecd1245e`, was consulted as an independent semantic reference, not copied as implementation source. Its conformance-restricted source license is not asserted to be MIT/LGPL. Numerical filter coefficients are standard data; the fork's existing FFmpeg current filters are reindexed and the symmetric previous-window coefficients are added. The implementation remains independent safe Rust in the existing LGPL-derived decoder.

Relevant behavior in `mpegD_usac/usacEncDec/src_usac/`:

- `usac_cplx_pred.c`: integer per-band alpha history from the last parsed group, zero imaginary alpha when `complex_coef=0`, real downmix chosen by prediction use, fresh per-window MDST with distinct current/previous filters and previous final spectra.
- `decode_chan_ele.c`: TNS ordered by `tns_on_lr`, including independent windows; save final spectra after TNS/stereo.
- `usac_fd_dec.c:usacMapWindowSequences`: coded LONG_START after short/start maps to STOP_START (short overlap on both halves).
- `usac_arith_dec.c:applyScaleFactorsAndNf/esc_iquant`: a noise-filling level of zero still advances the PRNG; configuration, not level, enables the tool.

Before these fixes, the preserved Rust executable matched 7350 Hz ISO Cp/WinCp at negative SNR, WinTns at about 16 dB, and noise filling at about 37 dB; plain TNS remained about 116 dB. Independent native libxaac matched the same ISO references at approximately 103–107 dB. Those are actual decoder comparisons, not estimates from a Python codec replica.

These cases do not establish full USAC conformance: 768-line FD, LPD/ACELP, FAC, eSBR, MPS212, time-warped MDCT, LFE/multichannel and DRC processing remain outside this implementation. Codec tests do not implement engine/container presentation trimming or prove phone performance.
