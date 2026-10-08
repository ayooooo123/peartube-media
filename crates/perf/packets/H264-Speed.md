# H264-Speed

## OBJ
Software H.264 is below the floors: 1080p30 High decodes at 0.35x real time
(CPU time, load 36; 794 G instructions for 30 s) and the FATE 854x480 stream
at 2.26x (load 38). Floors on this Mac: 2x for 1080p, 4x for SD. Every frame
already matches FFmpeg (900/900 and 5401/5401 MD5). Reach the floors without
changing a pixel. The app plays H.264 through MediaCodec or VideoToolbox when
it can; this decoder runs when the hardware refuses a stream.

Profile (`crates/perf/results/profiles/h264.top.txt`; the full `sample`
report is `~/projects/peartube-media-wt/.targets/perf-audit/profiles/h264.sample.txt`;
oxideav-h264 `1037432`, shipping profile, 13,482 samples):
- Deblocking, about 26%: `deblock_picture` (`src/reconstruct.rs:8258`, the
  inlined luma pass, 6.8%; `:8269`, chroma, 6.0%), `deblock::filter_edge`
  (7.1%, `src/deblock.rs:400`), boundary strength in
  `reconstruct::different_ref_or_mv_luma` (6.1%). Each edge sample goes
  through generic per-sample code.
- Motion compensation, about 13%: `simd::chunked::interpolate_luma` (8.9%,
  `src/simd/chunked.rs:371` builds a horizontal FIR strip for every block,
  even for full-pel positions) and `interpolate_chroma` (4.0%).
- Allocation and copies, about 12%: `memmove` 6.9%, `free` 2.4%, `malloc`
  1.2%, `memset` 1.2%. The first audit traced these to per-partition
  buffers in `reconstruct::process_partition` (now 7.0% self).
- `getenv` once per macroblock, 1.9%: `src/slice_data.rs:318` and `:380`
  read `OXIDEAV_H264_BIN_TRACE` / `OXIDEAV_H264_SKIP_TRACE` with
  `std::env::var_os` inside the macroblock loop; `:290` once per slice.
- CABAC `decode_decision` 4.1%; transforms about 3%.

FFmpeg 2da55bf (LGPL-2.1-or-later) filters a macroblock's edges with fixed
kernels per strength (`libavcodec/h264_loopfilter.c:234`
`h264_filter_mb_fast_internal`, `:468` `filter_mb_dir`; NEON in
`aarch64/h264dsp_neon.S`), interpolates per block size and quarter-pel
position (`h264_mc_template.c`, `aarch64/h264qpel_neon.S`), and keeps
prediction scratch in the slice context.

## OWN
Fork `ayooooo123/oxideav-h264` (branch `peartube`, from `1037432`), its own
worktree and target dir. Steps, each measured before the next:
1. Read the trace variables once (a `LazyLock<bool>` per variable).
2. Slice- or decoder-owned scratch for prediction and residual buffers; no
   heap allocation per partition or macroblock.
3. Deblocking: compute boundary strengths per edge once; filter with
   fixed-length kernels over rows and columns of the picture, not per
   sample through bounds-checked accessors. Then NEON for the bS < 4 and
   bS = 4 luma filters, as FFmpeg's.
4. MC: skip the FIR strip for full-pel and pure half-pel positions; a fast
   path for blocks inside the reference, edge emulation only at borders.
Keep bit depths, 4:2:2/4:4:4, MBAFF/PAFF, weighted prediction and every
oracle unchanged.

## VERIFY
- The fork's suite and `crates/check-decoders` H.264 tests, unchanged.
- `cargo run --release -p perf -- --filter "std:h264" --filter video:h264`:
  every MD5 still matches; instructions per run reported before and after
  each step; 1080p at or above 2x and 480p at or above 4x on a quiet Mac.
- Re-profile and report the new top five.
