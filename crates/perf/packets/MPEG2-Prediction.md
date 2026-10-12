# MPEG2-Prediction

## OBJ
MPEG-2 HD is below the 2x floor: `perf/mpeg2_720p30.ts` decodes at 1.17x
real time (CPU time, load 41; 135 G instructions for 20 s) and the FATE
XDCAM 1920x1080 MXF at 0.72x. SD passes (576i: 4.18x, 576p in MXF: 7.5x).
All 600 frames of the 720p input match FFmpeg. The first audit's VLC table
walk is gone (`2484f1d`); motion compensation is now the cost. Reach 2x at
720p and 1080 without changing a pixel.

Profile (`crates/perf/results/profiles/mpeg2.top.txt`; full report
`~/projects/peartube-media-wt/.targets/perf-audit/profiles/mpeg2.sample.txt`,
oxideav-mpeg12video `9c4c24c`, 15,570 samples):
- `forming_predictions::predict_block` 34.5%, 32.4% on
  `src/forming_predictions.rs:408`, plus `ReferencePlane::sample` 7.8%
  (`:311`): each block is a new `Vec<u8>`, and every sample goes through
  `predict_sample` (`:346`), which matches the half-pel pattern per sample
  and clamps both coordinates of each of up to four reads.
- `inter_reconstruction::write_inter_block` 7.6% (`src/inter_reconstruction.rs:1972`):
  per sample `mb_local_sample` + `put_sample`.
- `slice_macroblock_walk::walk_slice_at` 11.1% (`src/slice_macroblock_walk.rs:1468`).
- Motion vector parse 5.5%, IDCT 4.4%, coefficient VLC 3.9%, `free` 2.3%,
  `average_predictions` 2.2%.

FFmpeg 2da55bf (LGPL-2.1-or-later) predicts a whole block with one
`put_pixels_tab[size][dxy]` call chosen once per block
(`libavcodec/mpegvideo_motion.c:39` `hpel_motion`, `mpeg_motion_internal`),
copies through `emulated_edge_mc` only when the block reaches past the
picture (`:62`, `:165`), and averages B predictions with `avg_pixels_tab`;
NEON in `aarch64/hpeldsp_neon.S`.

## OWN
Fork `ayooooo123/oxideav-mpeg12video` (branch `peartube`, from `9c4c24c`),
its own worktree and target dir.
1. `predict_block` writes into a caller-owned buffer; the half-pel pattern
   is chosen once per block; rows are read directly when the block's source
   window is inside the plane (frame or field view), per sample with
   clamping otherwise. The same fast path is used in `oxideav-h263`
   `f9a411d` (`motion_compensate_block`).
2. `write_inter_block`: add and saturate a row at a time into the plane.
3. Re-profile; then the slice walk and IDCT.
Keep field/frame/dual-prime prediction, 4:2:2 and the oracle unchanged.

## VERIFY
- The fork's suite and the media MPEG-1/2 tests, unchanged.
- `cargo run --release -p perf -- --filter "std:mpeg2" --filter XDCAM --filter mxf`:
  every MD5 still matches, instructions before and after each step, 720p
  and 1080 at 2x or more on a quiet Mac.
- Separate correctness lead found here: the XDCAM 1080 MXF's frames are
  1920x1080 while the reported layout needs at least 1081 rows
  (`refcheck::pack` panics, "range end index 2075520 out of range for slice
  of length 2073600"); the reported height is 1088. Fix or hand it to the
  MXF/MPEG-2 owner.
