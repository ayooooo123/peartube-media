# Opt-levels of the new decoders (PerfAudit, 2026-10-08)

Rule (JD): keep "s" where the margin over the floor is ample, 3 where it is
needed. Floors on this Mac (M1 Pro): audio 10x real time, video above 576
lines 2x, SD video 4x. Speeds are CPU-time x real time (`perf`, median of 3
runs) under the shared load shown; instruction counts do not depend on load.

## Decisions

| crate | before | after | why |
|---|---|---|---|
| codec-dv | "s" | 3 | DVCPRO HD 1440x1080 at "s": 2.20x (load 32), at the 2x HD floor. At 3: 28% fewer instructions, 3.57x (load 13). DV25 576i: 8.47x to 15.67x. |
| oxideav-opus (port) | 3 | "s" | 7.1 at "s": 53.7x for the shipped `5573fa9` (load 15), 55.3x for `6a27d5b` (load 42); stereo 183x / 255x. 5x the floor or more. |
| oxideav-ac3 (port) | 3 | "s" | E-AC-3 7.1 at "s": 175x (load 14) / 138x (load 42); AC-3 5.1 330x. |
| oxideav-h263 | "s" | "s" | Fixed in code instead (`f9a411d`): a 704x576 stream takes 5.2x fewer instructions; 3.97x (load 37, below 4x) before, 21.4x (load 13) after. |
| codec-vp3, codec-vp56, codec-speex, codec-speech, codec-atrac, codec-apple-audio, oxideav-mp2, demux-mxf | "s" | "s" | Lowest: VP3 640x272 23.5x, VP5 512x304 26.6x, MP2 168x, ALAC 156x, QDM2 235x (floors 4x / 10x). |

Size (APK projection, method of SizeAudit: dx build of the app's libmain,
`llvm-objcopy --strip-all`, zlib level 6): app HEAD `91ff4f226` libmain
19,190,272 B stripped / 8,561,727 B deflated; with these opt-levels
19,144,608 / 8,538,300: **-45,664 B raw, -23,427 B in the APK**. Per crate
(macOS arm64 text, same builds otherwise): Opus port -39,412 B, AC-3
-34,836 B, codec-dv +21,040 B; the H.263 change +760 B.

## New decoders, "s" against 3 (before the H.263 fix)

Alternating runs of two builds that differ only in these crates' opt-level
(load 31-45).

| input | codec | instr s (G) | instr 3 (G) | 3/s instr | cpu ×RT s | cpu ×RT 3 | floor |
|---|---|---:|---:|---:|---:|---:|---:|
| perf/alac_stereo.m4a | alac | 1.398 | 0.972 | 0.695 | 169.8 | 223.3 | 10 |
| fate:amrnb/10.2k.amr | amr_nb | 0.033 | 0.027 | 0.814 | 1918.9 | 1964.5 | 10 |
| fate:amrwb/deus-23k85.awb | amr_wb | 0.771 | 0.602 | 0.781 | 435.3 | 488.7 | 10 |
| fate:atrac1/chirp_tone_10-16000.aea | atrac1 | 0.262 | 0.202 | 0.772 | 560.7 | 549.0 | 10 |
| fate:atrac3p/at3p_sample1.oma | atrac3plus | 0.688 | 0.501 | 0.728 | 294.3 | 366.3 | 10 |
| fate:dv/dvcprohd_1080i50.mov (1 frame) | dvvideo | 0.140 | 0.100 | 0.717 | 2.3 | 3.5 | 2 |
| perf/dv_576i25.avi | dvvideo | 17.143 | 12.745 | 0.743 | 8.4 | 11.0 | 4 |
| fate:mxf/Avid-00005.mxf | dvvideo | 0.745 | 0.505 | 0.678 | 11.8 | 20.1 | 4 |
| perf/h263_cif.avi | h263 | 19.741 | 16.889 | 0.856 | 11.6 | 11.7 | 4 |
| gen:video_h263i.avi | h263 | 1.725 | 1.446 | 0.838 | 31.9 | 42.4 | 4 |
| fate:qt-surge-suite/surge-1-8-MAC3.mov | mace3 | 0.082 | 0.036 | 0.435 | 1458.2 | 3749.4 | 10 |
| fate:qt-surge-suite/surge-1-8-MAC6.mov | mace6 | 0.069 | 0.032 | 0.455 | 1853.4 | 4537.7 | 10 |
| perf/mpeg2_pcm.mxf (PCM: the MXF demuxer) | pcm_s16le | 0.012 | 0.011 | 0.935 | 8961.0 | 10122.7 | 10 |
| fate:qcp/0036580847.QCP | qcelp | 0.035 | 0.026 | 0.751 | 3905.6 | 4353.2 | 10 |
| fate:qt-surge-suite/surge-2-16-B-QDM2.mov | qdm2 | 0.686 | 0.429 | 0.625 | 234.7 | 328.1 | 10 |
| fate:vp5/potter512-400-partial.avi | speex | 0.180 | 0.118 | 0.653 | 589.3 | 885.9 | 10 |
| fate:vp3/vp31.avi | vp3 | 1.466 | 1.289 | 0.879 | 23.5 | 30.3 | 4 |
| fate:vp5/potter512-400-partial.avi | vp5 | 3.277 | 2.474 | 0.755 | 26.6 | 44.0 | 4 |
| fate:flash-vp6/300x180-Scr-f8-056alpha.flv | vp6a | 0.797 | 0.694 | 0.871 | 239.0 | 336.5 | 4 |
| fate:flash-vp6/clip1024.flv | vp6f | 0.349 | 0.283 | 0.811 | 353.7 | 467.3 | 4 |
| gen:mpeg2_mp2.mpg (control, 3 in both) | mpeg2video | 1.295 | 1.299 | 1.003 | 38.7 | 50.5 | 4 |

MP2 in Matroska (`perf/mp2_stereo.mka`): 168.5x at "s" (load 32), 241.4x at
3 (load 38).

## Opus port `6a27d5b` and AC-3 port, 3 against "s"

Three alternating rounds (load 39-45).

| input | codec | instr 3 (G) | instr s (G) | s/3 | cpu ×RT 3 | cpu ×RT s |
|---|---|---:|---:|---:|---:|---:|
| perf/ac3_51.ac3 | ac3 | 1.126 | 1.363 | 1.210 | 281.7 | 244.2 |
| fate:ogg-ogm/bots01.ogm | ac3 | 0.339 | 0.409 | 1.207 | 639.5 | 517.2 |
| perf/eac3_71.eac3 | eac3 | 1.696 | 2.049 | 1.208 | 162.8 | 138.1 |
| fate:opus/test-8-7.1.opus-small.ts | opus | 1.104 | 1.594 | 1.444 | 80.4 | 55.3 |
| perf/opus_stereo.opus | opus | 0.640 | 0.871 | 1.361 | 339.3 | 254.9 |

## Shipping profile against the decided one (`57e675d` base)

| input | before: Ginstr, cpu ×RT (load) | after: Ginstr, cpu ×RT (load) | instr after/before |
|---|---|---|---:|
| perf/h263_4cif.avi (704x576) | 56.21, 3.97 (37) | 10.79, 21.43 (13) | 0.192 |
| perf/h263_cif.avi | 19.71, 10.64 (38) | 3.34, 66.33 (13) | 0.169 |
| gen:video_h263i.avi (176x144) | 1.72, 42.04 (38) | 0.19, 289.53 (15) | 0.111 |
| perf/dvcprohd_1080i50.mov | 38.85, 2.20 (32) | 28.26, 3.57 (13) | 0.727 |
| perf/dv_576i25.avi | 17.05, 8.47 (34) | 12.67, 15.67 (14) | 0.743 |
| fate:opus/test-8-7.1.opus-small.ts (`5573fa9`) | 1.81, 48.85 (39) | 2.77, 53.65 (15) | 1.534 |
| perf/opus_stereo.opus (`5573fa9`) | 1.16, 220.25 (37) | 1.71, 182.82 (15) | 1.474 |
| perf/ac3_51.ac3 | 1.12, 253.18 (27) | 1.36, 329.84 (14) | 1.207 |
| perf/eac3_71.eac3 | 1.70, 158.21 (27) | 2.04, 175.04 (14) | 1.203 |
