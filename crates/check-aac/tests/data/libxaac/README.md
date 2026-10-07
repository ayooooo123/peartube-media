# Native xhe reference

The parent explicitly selected unmodified [libxaac](https://github.com/ittiam-systems/libxaac) as the oracle for FATE `aac/usac/xhe_target_level.m4a`. FFmpeg's configuration parser turns AudioPreRoll into fill and leaves channel 0's noise seed at zero; its PCM is not the reference for this path. No other existing FFmpeg floor changes.

Source revision: `2fbadd57bca46693b7a077bcf1513131106d960c`. Decoder source and build inputs were unmodified. Its source license is Apache-2.0 (including `decoder/ixheaacd_ext_ch_ele.c`); no libxaac source is copied into the fork. These WAVs are derived test-only PCM from the existing FATE media, not app assets or a relicensing of that media. The Python extractor is project-authored, MIT.

## Reproduce on this Mac

```sh
git clone https://github.com/ittiam-systems/libxaac /Users/jd/projects/oracles/libxaac
git -C /Users/jd/projects/oracles/libxaac checkout 2fbadd57bca46693b7a077bcf1513131106d960c
cmake -S /Users/jd/projects/oracles/libxaac \
  -B /Users/jd/projects/oracles/libxaac-build-macos-c \
  -DCMAKE_BUILD_TYPE=Release '-DCMAKE_C_FLAGS=-D_X86_ -U__ARM_NEON__'
cmake --build /Users/jd/projects/oracles/libxaac-build-macos-c --target xaacdec -j 2

# From this directory. The output directory must already exist.
python3 mp4_es.py /Users/jd/projects/fate-suite/aac/usac/xhe_target_level.m4a /tmp/xhe
/Users/jd/projects/oracles/libxaac-build-macos-c/xaacdec \
  -ifile:/tmp/xhe.raw -imeta:/tmp/xhe.meta -ofile:/tmp/xhe_target_level.wav \
  -mp4:1 -pcmsz:24 -dmix:0 -tostereo:0 -peak_limiter_off:1 -err_conceal:0
/Users/jd/projects/oracles/libxaac-build-macos-c/xaacdec \
  -ifile:/tmp/xhe.raw -imeta:/tmp/xhe.meta -ofile:/tmp/xhe_target_level.t-24.wav \
  -mp4:1 -pcmsz:24 -dmix:0 -tostereo:0 -target_loudness:-24 \
  -peak_limiter_off:1 -err_conceal:0
cmp /tmp/xhe_target_level.wav xhe_target_level.wav
cmp /tmp/xhe_target_level.t-24.wav xhe_target_level.t-24.wav
```

The portable C selection avoids libxaac's Apple `arm64` CMake architecture mismatch. A second clean build reproduced the decoder binary and both WAVs byte for byte during the investigation. Binary SHA-256 is host/toolchain-specific; PCM and input hashes are the reference contract.

|Object|SHA-256|
|---|---|
|canonical M4A|`0acf6919e9a4cf25eed26be5aba5d305a33cf39bc4175658e979b8653404acee`|
|ASC + every AU (`xhe.raw`)|`c12802e13a6ce7217f55203a7017d3d5203c90c58c61b9dd135b2fa004d8fd8e`|
|untrimmed metadata (`xhe.meta`)|`cb023c1b471463123a272c9b9a11f6ef8eb61ea344dc4d79dafaf84d7a0fd31b`|
|`xhe_target_level.wav` (normalization off)|`f0be29d135ea7539026a8aac9b6894767ca4f471f6ca5226769c8df2af5913b9`|
|`xhe_target_level.t-24.wav`|`ec5122612d3a67633f394c60033f959533035595bc29c1d388941302648ed966`|
|`xaacdec`|`d7ef3ff532c3839eb386c5fc3edbe727da63489ccedef01b33a7b0a41b17ee54`|

ASC: `f94643221cc058520020000a4046d0b800` (17 bytes), followed by 48 access units. `mp4_es.py` traverses sample tables, never edits an AU or strips AudioPreRoll. The regression separately hashes the canonical M4A and the concatenated ASC/AUs emitted by the production OxideAV demuxer.

## Counts and floors

Both WAVs retain the native decoder's exact output, including its incorrect header: 50,176 declared frames versus **49,152 physically present** stereo frames, 24-bit, 48 kHz. The reader never fabricates the extra 1024 PCM frames. MP4 `elst` is `(duration=48000, media_time=0)` at timescale 48000; presented PCM is exactly frames `[0,48000)`. Raw surplus is 1152/channel: 128 from the preceding AU plus the entire final 1024-sample AU outside the edit.

Per-channel measurements on predecessor `02ed7a8`, in dB (test floors are each number minus 0.5):

|Target|Full raw L/R|Presented L/R|
|---|---|---|
|off|115.134525 / 115.380771|115.146389 / 115.394712|
|-24|112.035669 / 112.189522|112.089333 / 112.245927|

No gain fitting, alignment search, skipped startup, modified configuration, modified canonical packets, or shortened raw comparison. At target -24 the parent measured native versus unprimed Rust/FFmpeg at only 19.457726 / 20.035864 dB over presented PCM. This discrepancy is documentation, not a test pin to buggy FFmpeg output.

AU47's 84.14/87.65 dB is not evidence of a predictor defect: reference RMS is 0.001364/0.002045, error RMS approximately 8.47e-8, maximum below 1.8 native 24-bit LSB. It remains included in the full raw comparison. Broader complex-prediction and TNS verification uses separate [ISO fixtures](../iso-usac/README.md).
