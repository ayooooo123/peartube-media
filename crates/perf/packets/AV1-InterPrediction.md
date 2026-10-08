# AV1-InterPrediction

## OBJ
Software AV1 decodes `perf/av1_1080p30.mkv` at 0.22x real time (CPU time,
load 36; 1,400 G instructions for 30 s), against a 2x floor for 1080p. All
900 frames match libdav1d (FFmpeg's AV1 decoder; the pinned FFmpeg build has
no software AV1). Reach 2x without changing a pixel. Unchanged since the
first audit (0.33x wall then): oxideav-av1 `cab66b0` only added frame layout
reporting.

Profile (`crates/perf/results/profiles/av1.top.txt`; full report
`~/projects/peartube-media-wt/.targets/perf-audit/profiles/av1.sample.txt`,
13,253 samples):
- `inter_pred::block_inter_prediction` 39.8%, 28.6% on `src/inter_pred.rs:779`
  and `:780`: the horizontal pass of the 8-tap filter computes the phase
  per output sample, clamps both coordinates of every tap (`clip3_i32`) and
  sums in i64; the intermediate is a fresh `Vec<i32>` per block (`:750`).
  The vertical pass (`:813`) has the same form.
- `cdf::PartitionWalker::predict_inter_leaf_from_walk` 10.1%:
  `src/cdf.rs:19452` widens the whole reference region to u16 scratch per
  leaf, one sample at a time.
- CDEF 15%: `cdef::cdef_filter_block` 9.7% (`src/cdef.rs:588`) and
  `cdef_frame_from_idx` 5.2% (`src/cdf.rs:20745`, the inlined frame driver
  with a per-cell `skip` closure).
- Inverse DCT 5.6%, loop filter 2.4%, Wiener restoration 2.2%.

libdav1d (BSD-2-Clause) handles unscaled prediction with fixed 8-tap row and
column kernels per filter pair (`src/mc_tmpl.c` `put_8tap`/`prep_8tap`,
NEON in `src/arm/64/mc.S`), uses edge emulation only for blocks that cross
the reference border (`emu_edge`), and runs CDEF per 8x8 block with
precomputed directions (`src/cdef_tmpl.c`, `src/arm/64/cdef.S`).

## OWN
Fork `ayooooo123/oxideav-av1` (branch `peartube`, from `cab66b0`), its own
worktree and target dir.
1. Unscaled path: hoist the phase (constant per block when unscaled), read
   rows directly when the block's source window is inside the reference,
   i32 accumulators (prove the bound per bit depth first), reuse one
   intermediate buffer per decoder. Keep the general scaled and edge path.
2. Prediction scratch: copy or widen rows with slice operations, not per
   sample; or predict from the u8 reference directly.
3. CDEF: per-block direction search and filter over contiguous rows; drop
   the per-cell closure in the frame driver.
4. Then NEON for the 8-tap kernels.
Keep compound, warped, OBMC and scaled references exact.

## VERIFY
- The fork's suite, unchanged.
- `cargo run --release -p perf -- --filter "std:av1" --filter video:av1`:
  900/900 MD5 against libdav1d, instructions before and after each step, 2x
  or more on a quiet Mac.
- Re-profile and report the new top five.
