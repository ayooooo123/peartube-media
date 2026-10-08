# MPEG4-QPel

## OBJ
MPEG-4 ASP with quarter-pel motion is below the SD floor under load:
`fate:ogg-ogm/bots01.ogm` (640x480, qpel) decodes at 3.52x real time (CPU
time, load 43; 74.5 G instructions for 22 s) against 4x; all 667 frames
match FFmpeg. Without qpel the decoder passes (Xvid 848x480: 5.67x, load 33).
The quiet-window number decides whether this ships as is; the waste below is
worth removing either way. Keep every pixel.

Profile (`crates/perf/results/profiles/mpeg4.top.txt`; full report
`~/projects/peartube-media-wt/.targets/perf-audit/profiles/mpeg4.sample.txt`,
oxideav-mpeg4video `a759321`, 15,846 samples): quarter-pel interpolation is
about 71% of self time.
- `quarter_sample::horiz_taps` 31.5%, 26.6% on `src/quarter_sample.rs:364`:
  eight `fetch` calls (mirror-clamped) per half-pel sample.
- `interpolate_quarter_pixel_src` 11.9%, `interpolate_block_qpel_into` 8.3%
  (`:826`, `:835`), `compute_k` 7.0% (`:648`), `compute_l` 6.7% (`:665`),
  `half_pel_d_src` 3.2%, `vert_taps` 2.1%.
Each output sample recomputes up to eight horizontal 8-tap half-pel values
(`compute_k`/`compute_l` build an 8-row column of them, `:640-668`), so a
block costs about eight times the work of a separable pass, and nothing is
shared between neighbouring outputs.

FFmpeg 2da55bf (LGPL-2.1-or-later) filters the mirrored (w+1)x(h+1) block
once horizontally into a row buffer and once vertically
(`libavcodec/qpeldsp.c`, the `put_mpeg4_qpel8_h_lowpass` /
`_v_lowpass` pairs behind each `qpel_mc` position), chosen once per block
in `mpegvideo_motion.c:337` `qpel_motion`.

## OWN
Fork `ayooooo123/oxideav-mpeg4video` (branch `peartube`, from `a759321`),
its own worktree and target dir.
1. Per block: read the mirrored (w+1)x(h+1) source once (already done by
   `MirroredRefBlock::read`), run the horizontal 8-tap lowpass over every
   row it needs into a scratch buffer, then the vertical pass, then the
   bilinear quarter averages, all per block and per position class, as
   FFmpeg's `qpel_mc` table does. Keep rounding control and the mirror rule.
2. Reuse the scratch per decoder; no allocation per block.
Keep GMC, interlaced and 4MV paths exact.

## VERIFY
- The fork's suite and `crates/check-mpeg4`, unchanged.
- `cargo run --release -p perf -- --filter video:mpeg4 --filter "std:mpeg4"`:
  every MD5 still matches, instructions before and after, 4x or more for
  bots01 on a quiet Mac.
