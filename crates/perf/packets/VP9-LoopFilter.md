# VP9-LoopFilter

## OBJ
Software VP9 decodes `perf/vp9_1080p30.webm` at 0.22x real time (CPU time,
load 46; 1,306 G instructions for 30 s), against a 2x floor for 1080p. All
900 frames match FFmpeg. Reach 2x without changing a pixel. Unchanged since
the first audit (0.32x wall then): oxideav-vp9 `35a7c43` only added frame
layout reporting.

Profile (`crates/perf/results/profiles/vp9.top.txt`; full report
`~/projects/peartube-media-wt/.targets/perf-audit/profiles/vp9.sample.txt`):
- The loop filter is 67.7% of self time, 53.8% on one line,
  `src/superblock_loop_filter.rs:399`: for every edge position the filter
  gathers a 16-sample stencil (`gather_stencil`, i64 coordinates), runs
  `sample_filtering` (`src/sample_filtering.rs:256`; the wide case in
  `src/wide_filter.rs` sums up to 15 clamped taps per output) and scatters
  it back (`:407`). Edge bundles are built per 4x4 position (`:348`).
- Inter prediction `predict_inter_region`, 14.8%
  (`src/decode_frame.rs:1074`, the inlined `predict_inter`).
- Tokens 2.6%, IDCT about 3.6%.

FFmpeg 2da55bf (LGPL-2.1-or-later) filters whole 8-sample edges with one
fixed kernel per width (`libavcodec/vp9dsp_template.c:1780` `loop_filter`,
`:1891` `lf_8_fn`, 4/8/16-wide, horizontal and vertical), chosen per edge
from masks built once per superblock (`vp9lpf.c`), with NEON in
`aarch64/vp9lpf_neon.S`; MC uses fixed 8-tap kernels per block width
(`aarch64/vp9mc_neon.S`).

## OWN
Fork `ayooooo123/oxideav-vp9` (branch `peartube`, from `35a7c43`), its own
worktree and target dir.
1. Build the per-superblock filter masks once, then filter each edge as a
   run of 8 rows or columns in place: no gather/scatter, no i64 per-sample
   coordinates, separate kernels for the 4/8/16 widths.
2. NEON (or `core::arch::aarch64` intrinsics) for the three kernels once the
   scalar run form is correct.
3. Then MC: fixed-tap row kernels for blocks inside the reference, edge
   emulation only at borders.
Keep segmentation levels, high bit depth and filter ordering.

## VERIFY
- The fork's suite, unchanged.
- `cargo run --release -p perf -- --filter "std:vp9" --filter video:vp9`:
  900/900 MD5, instructions before and after each step, 2x or more on a
  quiet Mac.
- Re-profile and report the new top five.
