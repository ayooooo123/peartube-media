# AAC-IndependentWindows (P1, correctness)

## OBJ
oxideav-aac (`d9f6d8e`, the pinned fork) refuses some access units of plain
AAC-LC that FFmpeg's own `aac` encoder writes, which is the most common AAC
encoder outside Apple and FDK:
- `perf/aac_lc_stereo.m4a` (160 kbit/s stereo): 10 of 1,408 access units;
- `perf/aac_lc_51.m4a` (384 kbit/s 5.1): 13 of 1,407.
Each fails with `ElementDecodeInvalid` ("channel-element component shapes
(window_sequence pairing, ms_used extent, or scalefactor-record count) are
mutually inconsistent"). FFmpeg decodes every frame. The player drops each
refused frame, so the user hears a gap of about 21 ms at each.
Both inputs come from `corpus/perf-inputs.sh audio`.

Lead (from the code; not traced frame by frame):
`ElementDecoder::decode_cpe_coupled` (`src/element_decode.rs:783-787`)
returns `ElementDecodeInvalid` whenever the two channels of a CPE differ in
`window_sequence`, `num_window_groups` or `window_group_length`. Its own doc
comment (`:750-752`) limits that rule to pairs where a joint-stereo tool is
active. A CPE with `common_window == 0` carries one `ics_info` per channel and
may switch one channel to EIGHT_SHORT on a transient while the other stays
long; no M/S or intensity runs on such a pair (ISO 14496-3 §4.6.8.1). The
refused units are likely those transients.

## OWN
Fork `ayooooo123/oxideav-aac`, branch `peartube`, its own worktree. First
confirm the lead: log `common_window` and both window sequences for the
refused units of the two inputs. Then apply the geometry rule only where
the joint-stereo tools need it (`common_window == 1`, or any active M/S or
intensity band), as FFmpeg's `decode_cpe` (`libavcodec/aac/aacdec.c`) does.

## VERIFY
- A test with a CPE of independent windows (one long, one eight-short,
  `common_window == 0`) that decodes like FFmpeg.
- `cargo run --release -p perf -- --filter "std:aac"`: no decode errors,
  complete PCM at 90 dB or better against FFmpeg 2da55bf for stereo and 5.1.
- The fork's suite and the media AAC checks (`crates/check-aac`), unchanged.
