- aee9369 shipping: commit aee936937, 122 inputs, load 26-55
- 57e675d decided: commit b2abcfac4, 77 inputs, load 13-15

| verdict | cpu ×RT | Ginstr | floor | codec → decoder | input | shape | media s | check | run, load |
|---|---:|---:|---:|---|---|---|---:|---|---|
| SLOW! | 0.97 | 10.3 | 2 | mpeg2video → mpeg2video_sw | fate:mxf/omneon_8.3.0.0_xdcam_startc_footer.mxf | 1920x1088 | 0.7 | panicked: range end index 2075520 out of range for slice of  | 57e675d decided 15 |
| SLOW | 0.22 | 1305.6 | 2 | vp9 → oxideav-vp9 | perf/vp9_1080p30.webm | 1920x1080 | 30.0 | 900/900 MD5 | aee9369 shipping 46 |
| SLOW | 0.22 | 1400.0 | 2 | av1 → av1_sw | perf/av1_1080p30.mkv | 1920x1080 | 30.0 | 900/900 MD5 | aee9369 shipping 36 |
| SLOW | 0.35 | 794.0 | 2 | h264 → h264_sw | perf/h264_1080p30_high.mp4 | 1920x1080 | 30.0 | 900/900 MD5 | aee9369 shipping 36 |
| SLOW | 1.17 | 134.9 | 2 | mpeg2video → mpeg2video_sw | perf/mpeg2_720p30.ts | 1280x720 | 20.0 | 600/600 MD5 | aee9369 shipping 41 |
| SLOW | 1.30 | 169.5 | 2 | h265 → h265 | perf/hevc_1080p30_main.mkv | 1920x1080 | 30.0 | 900/900 MD5 | aee9369 shipping 28 |
| SLOW | 1.41 | 175.8 | 2 | h265 → h265 | perf/hevc_1080p30_main10.mkv | 1920x1080 | 30.0 | 900/900 MD5 | aee9369 shipping 55 |
| SLOW | 2.26 | 803.2 | 4 | h264 → h264_sw | fate:mov/buck480p30_na.mp4 | 854x480 | 180.0 | 5401/5401 MD5 | aee9369 shipping 38 |
| SLOW | 3.52 | 74.5 | 4 | mpeg4video → mpeg4video_sw | fate:ogg-ogm/bots01.ogm | 640x480 | 22.3 | 667/667 MD5 | aee9369 shipping 43 |
| FAIL | 4.81 | 2.3 | 4 | dirac → dirac_sw | perf/dirac_rewrapped.mkv | 320x240 | 0.7 | 1/30 MD5 | 57e675d decided 14 |
| FAIL | 35.07 | 1.4 | 10 | wavpack → wavpack_sw | fate:wavpack/special/matroska_mode.mka | 8ch 48k | 5.5 | unknown sample format | 57e675d decided 14 |
| FAIL | 38.73 | 3.3 | 4 | vp5 → vp5_pear_sw | fate:vp5/potter512-400-partial.avi | 512x304 | 10.3 | 246/247 MD5 | 57e675d decided 15 |
| FAIL | 45.65 | 9.2 | 10 | aac → aac | perf/aac_lc_51.m4a | 6ch 48k | 29.7 | panicked: send_packet: invalid data: oxideav-aac: decode acc | 57e675d decided 14 |
| FAIL | 58.45 | 0.1 | 4 | indeo5 → indeo5_sw | fate:iv50/Educ_Movie_DeadlyForce.avi | 240x180 | 0.6 | 1/133 MD5 | 57e675d decided 14 |
| FAIL | 66.43 | 4.3 | 4 | svq3 → svq3_sw | perf/svq3_rewrapped.mov | 320x240 | 20.0 | 2/600 MD5 | 57e675d decided 14 |
| FAIL | 69.57 | 8.6 | 4 | svq3 → svq3_sw | fate:svq3/Vertical400kbit.sorenson3.mov | 320x240 | 43.6 | 3/1308 MD5 | 57e675d decided 15 |
| FAIL | 80.56 | 1.0 | 4 | mjpeg → mjpeg_sw | gen:video_mjpeg.avi | 320x240 | 6.0 | 0/150 MD5 | 57e675d decided 15 |
| FAIL | 104.21 | 4.2 | 10 | aac → aac | perf/aac_lc_stereo.m4a | 2ch 48k | 29.8 | panicked: send_packet: invalid data: oxideav-aac: decode acc | 57e675d decided 14 |
| FAIL | 150.92 | 0.7 | 4 | indeo3 → indeo3_sw | fate:iv32/OPENINGH.avi | 320x188 | 6.7 | 0/100 MD5 | 57e675d decided 15 |
| FAIL | 183.38 | 2.5 | 4 | svq1 → svq1_sw | fate:svq1/marymary-shackles.mov | 160x120 | 30.9 | 0/465 MD5 | 57e675d decided 15 |
| FAIL | 201.29 | 1.9 | 10 | mp2 → mp2 | perf/mp2_stereo.mka | 2ch 48k | 30.0 | 72.92 dB | 57e675d decided 14 |
| FAIL | 217.62 | 0.4 | 4 | h261 → h261_sw | gen:video_h261.avi | 176x144 | 6.0 | 0/90 MD5 | 57e675d decided 15 |
| FAIL | 224.81 | 1.5 | 4 | svq1 → svq1_sw | perf/svq1_rewrapped.mov | 160x120 | 20.0 | 0/300 MD5 | 57e675d decided 14 |
| FAIL | 291.31 | 0.5 | 10 | aac → aac | fate:flv/Enigma_Principles_of_Lust-part.flv | 2ch 22.05k | 11.7 | panicked: send_packet: invalid data: oxideav-aac: decode acc | 57e675d decided 15 |
| FAIL | 307.18 | 1.0 | 10 | mp3 → mp3 | perf/mp3_stereo.mp3 | 2ch 44.1k | 30.0 | 77.45 dB | 57e675d decided 15 |
| FAIL | 606.83 | 0.3 | 10 | mod → mod_sw | gen:audio_mod.mod | 2ch 44.1k | 7.7 | [in#0 @ 0x9e100c000] Error opening input: Invalid data found | 57e675d decided 14 |
| FAIL | 651.04 | 0.1 | 4 | cinepak → cinepak_sw | fate:cvid/laracroft-cinepak-partial.avi | 400x187 | 6.5 | 10/79 MD5 | 57e675d decided 15 |
| FAIL | 659.44 | 0.1 | 10 | mp3 → mp3 | gen:audio.mp3 | 1ch 48k | 6.0 | 79.61 dB | 57e675d decided 14 |
| FAIL | 3378.44 | 0.2 | 10 | adpcm_ima_qt → adpcm_ima_qt_sw | fate:svq3/Vertical400kbit.sorenson3.mov | 1ch 44.1k | 43.6 | 39.34 dB | 57e675d decided 14 |
| ok | 3.00 | 0.1 | 2 | dvvideo → dvvideo_sw | fate:dv/dvcprohd_1080i50.mov | 1440x1080 | 0.0 | 1/1 MD5 | aee9369 shipping 44 |
| ok | 3.57 | 28.2 | 2 | dvvideo → dvvideo_sw | perf/dvcprohd_1080i50.mov | 1440x1080 | 10.0 | 250/250 MD5 | 57e675d decided 13 |
| ok | 4.18 | 74.5 | 4 | mpeg2video → mpeg2video_sw | perf/mpeg2_576i25.m2v | 720x576 | 30.0 | 750/750 MD5 | aee9369 shipping 35 |
| ok | 5.67 | 61.4 | 4 | mpeg4video → mpeg4video_sw | perf/mpeg4_asp_480p30.avi | 848x480 | 30.0 | 900/900 MD5 | aee9369 shipping 33 |
| ok | 9.16 | 8.8 | 4 | vp9 → oxideav-vp9 | gen:vp9_opus.webm | 320x240 | 6.0 | 150/150 MD5 | 57e675d decided 15 |
| ok | 10.03 | 34.9 | 4 | mpeg1video → mpeg1video_sw | perf/mpeg1_480p30.m1v | 848x480 | 24.0 | 600/600 MD5 | 57e675d decided 15 |
| ok | 10.41 | 26.8 | 4 | mpeg2video → mpeg2video_sw | perf/mpeg2_pcm.mxf | 720x576 | 20.0 | 500/500 MD5 | 57e675d decided 15 |
| ok | 12.39 | 7.1 | 4 | av1 → av1_sw | gen:av1_opus.mkv | 320x240 | 6.0 | 150/150 MD5 | 57e675d decided 15 |
| ok | 15.67 | 12.7 | 4 | dvvideo → dvvideo_sw | perf/dv_576i25.avi | 720x576 | 20.0 | 500/500 MD5 | 57e675d decided 14 |
| ok | 15.74 | 16.3 | 10 | cook → codec-ra_cook | fate:real/spygames-2MB.rmvb | 2ch 44.1k | 20.4 | 124.92 dB | aee9369 shipping 35 |
| ok | 17.57 | 13.7 | 4 | rv40 → rv40_sw | fate:real/spygames-2MB.rmvb | 576x320 | 21.7 | 521/521 MD5 | aee9369 shipping 32 |
| ok | 21.43 | 10.8 | 4 | h263 → h263_sw | perf/h263_4cif.avi | 704x576 | 20.0 | 600/600 MD5 | 57e675d decided 13 |
| ok | 22.83 | 13.1 | 10 | aac → aac | fate:aac/al_sbr_cm_48_5.1.mp4 | 6ch 48k | 32.2 | 128.20 dB | 57e675d decided 14 |
| ok | 28.80 | 0.5 | 4 | dvvideo → dvvideo_sw | fate:mxf/Avid-00005.mxf | 720x576 | 1.0 | 25/25 MD5 | 57e675d decided 15 |
| ok | 29.43 | 0.4 | 4 | wmv3 → wmv3_sw | fate:vc1/SMM0015.rcv | 720x576 | 1.0 | 25/25 MD5 | 57e675d decided 14 |
| ok | 32.52 | 1.3 | 4 | rv30 → rv30_sw | fate:real/rv30.rm | 352x240 | 3.6 | 109/109 MD5 | aee9369 shipping 35 |
| ok | 39.67 | 1.5 | 4 | vp3 → vp3_sw | fate:vp3/vp31.avi | 640x272 | 4.6 | 114/114 MD5 | 57e675d decided 15 |
| ok | 42.76 | 0.9 | 4 | theora → theora | gen:video_theora.ogg | 320x240 | 6.0 | 150/150 MD5 | aee9369 shipping 35 |
| ok | 44.50 | 6.6 | 4 | wmv1 → wmv1_sw | perf/wmv1_480p30.avi | 848x480 | 20.0 | 600/600 MD5 | 57e675d decided 14 |
| ok | 49.92 | 1.6 | 4 | mpeg4video → mpeg4video_sw | gen:mpeg4_xvid.avi | 320x240 | 6.0 | 150/150 MD5 | aee9369 shipping 39 |
| ok | 53.65 | 2.8 | 10 | opus → opus_sw | fate:opus/test-8-7.1.opus-small.ts | 8ch 48k | 10.2 | 140.43 dB | 57e675d decided 15 |
| ok | 56.95 | 1.9 | 10 | wmalossless → wmalossless_sw_dec | fate:lossless-audio/luckynight-partial.wma | 2ch 44.1k | 9.8 | PCM exact | aee9369 shipping 37 |
| ok | 57.52 | 1.3 | 4 | mpeg2video → mpeg2video_sw | gen:mpeg2_mp2.mpg | 320x240 | 6.0 | 150/150 MD5 | 57e675d decided 15 |
| ok | 58.53 | 1.3 | 4 | mpeg1video → mpeg1video_sw | gen:mpeg1_mp2.mpg | 320x240 | 6.0 | 150/150 MD5 | 57e675d decided 15 |
| ok | 58.84 | 0.9 | 4 | vp8 → vp8 | gen:video_vp8.webm | 320x240 | 6.0 | 150/150 MD5 | aee9369 shipping 34 |
| ok | 61.61 | 4.7 | 10 | aac → aac | perf/he_aac_stereo.m4a | 2ch 48k | 30.2 | 135.94 dB | 57e675d decided 14 |
| ok | 66.07 | 1.0 | 4 | h265 → h265 | gen:hevc10_eac3.mkv | 320x240 | 6.0 | 150/150 MD5 | 57e675d decided 15 |
| ok | 66.33 | 3.3 | 4 | h263 → h263_sw | perf/h263_cif.avi | 352x288 | 20.0 | 600/600 MD5 | 57e675d decided 13 |
| ok | 70.75 | 1.4 | 10 | flac → flac_sw | perf/flac_24_96.flac | 2ch 96k | 30.0 | PCM exact | aee9369 shipping 28 |
| ok | 73.04 | 3.6 | 10 | aac → aac | perf/he_aac_v2_stereo.m4a | 2ch 48k | 30.2 | 128.96 dB | 57e675d decided 14 |
| ok | 76.22 | 4.6 | 10 | truehd → truehd_sw_dec | perf/truehd_71.thd | 8ch 48k | 26.7 | PCM exact | 57e675d decided 15 |
| ok | 94.74 | 2.3 | 10 | vorbis → vorbis_sw | perf/vorbis_stereo.ogg | 2ch 44.1k | 30.0 | 139.16 dB | aee9369 shipping 37 |
| ok | 107.48 | 2.1 | 4 | msmpeg4v3 → msmpeg4v3_pear_sw | fate:ogg-ogm/intro_ogg.ogm | 352x240 | 22.7 | 680/680 MD5 | aee9369 shipping 34 |
| ok | 110.11 | 3.2 | 10 | dts → dca_sw_dec | perf/dts_51.dts | 6ch 48k | 30.0 | 160.80 dB | aee9369 shipping 26 |
| ok | 111.18 | 1.8 | 10 | vorbis → vorbis_sw | fate:ogg-ogm/intro_ogg.ogm | 2ch 44.1k | 22.7 | 139.02 dB | aee9369 shipping 35 |
| ok | 148.54 | 0.3 | 4 | vc1 → vc1_sw | fate:isom/vc1-wmapro.ism | 240x104 | 5.0 | 120/120 MD5 | aee9369 shipping 43 |
| ok | 168.27 | 0.4 | 4 | wmv1 → wmv1_sw | gen:wmv1_wma1.asf | 320x240 | 6.0 | 150/150 MD5 | aee9369 shipping 34 |
| ok | 175.04 | 2.0 | 10 | eac3 → eac3_sw_dec | perf/eac3_71.eac3 | 8ch 48k | 25.1 | 140.09 dB | 57e675d decided 14 |
| ok | 182.82 | 1.7 | 10 | opus → opus_sw | perf/opus_stereo.opus | 2ch 48k | 30.0 | 139.43 dB | 57e675d decided 15 |
| ok | 211.70 | 1.6 | 4 | wmv2 → wmv2_sw | fate:wmv8/wmv8_x8intra.wmv | 320x240 | 31.6 | 474/474 MD5 | aee9369 shipping 34 |
| ok | 227.91 | 1.4 | 10 | alac → alac_sw | perf/alac_stereo.m4a | 2ch 44.1k | 30.0 | PCM exact | 57e675d decided 14 |
| ok | 232.48 | 0.4 | 10 | ralf → codec-ra_ralf | fate:lossless-audio/luckynight-partial.rmvb | 2ch 44.1k | 9.7 | PCM exact | aee9369 shipping 35 |
| ok | 235.75 | 0.9 | 4 | wmv3 → wmv3_sw | fate:vc1/SMM0005.rcv | 720x480 | 24.0 | 24/24 MD5 | aee9369 shipping 43 |
| ok | 249.47 | 1.2 | 10 | wmapro → wmapro_sw_dec | perf/wmapro_51.wma | 6ch 48k | 26.1 | 138.22 dB | aee9369 shipping 36 |
| ok | 289.53 | 0.2 | 4 | h263 → h263_sw | gen:video_h263i.avi | 176x144 | 6.0 | 90/90 MD5 | 57e675d decided 15 |
| ok | 316.15 | 0.8 | 4 | vp6a → vp6a_pear_sw | fate:flash-vp6/300x180-Scr-f8-056alpha.flv | 300x180 | 23.2 | 93/93 MD5 | 57e675d decided 15 |
| ok | 320.73 | 0.7 | 10 | qdm2 → qdm2_sw | fate:qt-surge-suite/surge-2-16-B-QDM2.mov | 2ch 44.1k | 12.8 | inf dB | 57e675d decided 14 |
| ok | 329.84 | 1.4 | 10 | ac3 → ac3_sw_dec | perf/ac3_51.ac3 | 6ch 48k | 30.0 | 140.13 dB | 57e675d decided 14 |
| ok | 337.04 | 0.3 | 10 | mlp → mlp_sw_dec | fate:lossless-audio/luckynight-partial.mlp | 2ch 44.1k | 8.1 | PCM exact | aee9369 shipping 37 |
| ok | 348.78 | 0.3 | 10 | dts → dca_sw_dec | gen:h264_dts.ts | 1ch 48k | 6.0 | inf dB | aee9369 shipping 36 |
| ok | 383.01 | 0.2 | 10 | tta → tta_sw | fate:lossless-audio/inside.tta | 2ch 44.1k | 11.9 | PCM exact | aee9369 shipping 34 |
| ok | 384.46 | 0.1 | 10 | wmapro → wmapro_sw_dec | fate:wmapro/Beethovens_9th-1_small.wma | 2ch 48k | 1.9 | 136.58 dB | aee9369 shipping 43 |
| ok | 409.85 | 0.7 | 10 | atrac3plus → atrac3plus | fate:atrac3p/at3p_sample1.oma | 2ch 44.1k | 20.9 | 137.13 dB | 57e675d decided 14 |
| ok | 497.10 | 0.1 | 10 | flac → flac_sw | gen:audio.flac | 1ch 48k | 6.0 | PCM exact | aee9369 shipping 35 |
| ok | 511.03 | 0.3 | 4 | vp6f → vp6f_pear_sw | fate:flash-vp6/clip1024.flv | 112x80 | 17.4 | 174/174 MD5 | 57e675d decided 15 |
| ok | 544.27 | 0.1 | 10 | truehd → truehd_sw_dec | gen:h264_truehd.ts | 1ch 48k | 6.0 | PCM exact | 57e675d decided 14 |
| ok | 577.27 | 0.8 | 10 | wmav1 → wmav1_sw_dec | perf/wma1_stereo.wma | 2ch 44.1k | 30.0 | 136.56 dB | 57e675d decided 14 |
| ok | 621.50 | 0.1 | 10 | alac → alac_sw | gen:audio_alac.m4a | 1ch 48k | 6.0 | PCM exact | 57e675d decided 14 |
| ok | 642.22 | 0.8 | 10 | amr_wb → amrwb_sw | fate:amrwb/deus-23k85.awb | 1ch 16k | 33.0 | inf dB | 57e675d decided 14 |
| ok | 701.98 | 0.3 | 10 | atrac1 → atrac1 | fate:atrac1/chirp_tone_10-16000.aea | 2ch 44.1k | 11.1 | 137.43 dB | 57e675d decided 15 |
| ok | 759.18 | 0.2 | 10 | speex → speex_sw | fate:vp5/potter512-400-partial.avi | 1ch 32k | 10.3 | inf dB | 57e675d decided 15 |
| ok | 767.68 | 0.1 | 10 | wmav1 → wmav1_sw_dec | gen:wmv1_wma1.asf | 1ch 48k | 6.0 | 136.32 dB | aee9369 shipping 34 |
| ok | 788.61 | 0.1 | 4 | vp6 → vp6_pear_sw | fate:vp6/interlaced32x32.avi | 32x32 | 6.6 | 151/151 MD5 | 57e675d decided 15 |
| ok | 807.29 | 0.4 | 10 | ac3 → ac3_sw_dec | fate:ogg-ogm/bots01.ogm | 2ch 48k | 22.4 | 139.66 dB | 57e675d decided 14 |
| ok | 889.99 | 0.0 | 10 | ra_288 → codec-ra_ra_288 | fate:realaudio/ra4_288.ra | 1ch 8k | 1.4 | 94.94 dB | aee9369 shipping 34 |
| ok | 1223.92 | 0.1 | 10 | wmavoice → wmavoice_sw_dec | fate:wmavoice/streaming_CBR-11K.wma | 1ch 8k | 16.5 | inf dB | aee9369 shipping 37 |
| ok | 1460.55 | 0.1 | 10 | eac3 → eac3_sw_dec | gen:hevc10_eac3.mkv | 1ch 48k | 6.0 | 139.46 dB | 57e675d decided 15 |
| ok | 1555.63 | 0.3 | 10 | sipr → codec-ra_sipr | fate:sipr/sipr_16k.rm | 1ch 16k | 33.6 | inf dB | aee9369 shipping 34 |
| ok | 1953.24 | 0.2 | 10 | wmav2 → wmav2_sw_dec | fate:wmv8/wmv8_x8intra.wmv | 1ch 16k | 35.0 | 137.27 dB | aee9369 shipping 34 |
| ok | 2193.60 | 0.1 | 10 | mace3 → mace3_sw | fate:qt-surge-suite/surge-1-8-MAC3.mov | 1ch 44.1k | 13.1 | inf dB | 57e675d decided 14 |
| ok | 2634.51 | 0.0 | 10 | amr_nb → amrnb_sw | fate:amrnb/10.2k.amr | 1ch 8k | 5.7 | inf dB | 57e675d decided 14 |
| ok | 2686.61 | 0.1 | 10 | mace6 → mace6_sw | fate:qt-surge-suite/surge-1-8-MAC6.mov | 1ch 44.1k | 13.1 | inf dB | 57e675d decided 14 |
| ok | 2957.44 | 0.1 | 4 | rv10 → rv10_sw | fate:sipr/sipr_5k0.rm | 160x112 | 19.2 | 77/77 MD5 | 57e675d decided 14 |
| ok | 3280.74 | 0.1 | 4 | rv20 → rv20_sw | fate:sipr/sipr_16k.rm | 128x96 | 21.0 | 42/42 MD5 | aee9369 shipping 34 |
| ok | 4308.09 | 0.0 | 10 | qcelp → qcelp_sw | fate:qcp/0036580847.QCP | 1ch 8k | 9.9 | inf dB | 57e675d decided 14 |
| ok | 5002.50 | 0.1 | 10 | dvaudio → dvaudio_sw | perf/dvaudio_ulead.wav | 2ch 48k | 20.0 | inf dB | 57e675d decided 15 |
| ok | 5484.23 | 0.3 | 10 | ra_144 → codec-ra_ra_144 | fate:real/ra3_in_rm_file.rm | 1ch 8k | 108.8 | PCM exact | aee9369 shipping 34 |
| ok | 13245.03 | 0.0 | 10 | pcm_s16le → pcm_s16le_sw | perf/mpeg2_pcm.mxf | 2ch 48k | 20.0 | PCM exact | 57e675d decided 15 |
| ok | 125000.00 | 0.0 | 10 | pcm_alaw → g711_alaw_sw | gen:audio_alaw.wav | 1ch 8k | 6.0 | PCM exact | aee9369 shipping 34 |
| ok | 150000.00 | 0.0 | 10 | pcm_mulaw → g711_mulaw_sw | gen:audio_ulaw.wav | 1ch 8k | 6.0 | PCM exact | aee9369 shipping 34 |
| ok | 179487.18 | 0.0 | 10 | pcm_u8 → pcm_u8_sw | fate:cvid/laracroft-cinepak-partial.avi | 1ch 8k | 7.0 | PCM exact | 57e675d decided 14 |
| ERROR |  |  | 10 | mp2 → mp2 | gen:mpeg1_mp2.mpg | 1ch 48k |  | oxideav-mp2: header parse: bitrate 384 kbit/s is not permitted with mode SingleChannel per | 57e675d decided 15 |
| ERROR |  |  | 4 | video:h263i |  | ?x? |  | gen:video_h263i.avi: no h263i stream (found h263) | 57e675d decided  |
| ERROR |  |  | 4 | video:dirac |  | ?x? |  | fate:dirac/vts.profile-main.drc: no container claims it | 57e675d decided  |
| ERROR |  |  | 10 | audio:musepack |  | ?ch 0k |  | fate:musepack/inside-mp7.mpc: no container claims it; fate:musepack/inside-mp8.mpc: no con | aee9369 shipping  |
| ERROR |  |  | 10 | atrac3 → atrac3 | fate:atrac3/mc_sich_at3_066_small.wav | 2ch 44.1k |  | invalid data: atrac3: unknown extradata size 0 | 57e675d decided 14 |
| ERROR |  |  | 10 | audio:wavpack |  | ?ch 0k |  | fate:wavpack/lossless/16bit-partial.wv: no container claims it | 57e675d decided  |
| ERROR |  |  | 10 | audio:ape |  | ?ch 0k |  | fate:lossless-audio/NoLegacy-cut.ape: no container claims it | aee9369 shipping  |
| ERROR |  |  | 10 | audio:midi |  | ?ch 0k |  | gen:audio.mid: no midi stream (found ) | aee9369 shipping  |
| ERROR |  |  | 10 | audio:dvaudio |  | ?ch 0k |  | fate:dv/dvcprohd_720p50.mov: no dvaudio stream (found ) | 57e675d decided  |
| ERROR |  |  | 4 | avi:3IV2 →  | perf/mpeg4_3ivx.avi | 848x480 |  | no decoder for avi:3IV2 | 57e675d decided  |
| ERROR |  |  | 4 | flv1 →  | perf/flv1_480p30.flv | 848x480 |  | no decoder for flv1 | 57e675d decided  |
| ERROR |  |  | 10 | mp3 → mp3 | perf/mp2_stereo.mp2 | 2ch 48k |  | unsupported: oxideav-mp3: decoder requires Layer III (got LayerII) | 57e675d decided 14 |
| MISSING |  |  | 4 | video:3ivx |  | ?x? |  | gen:mpeg4_3ivx.avi: missing file /Users/jd/projects/peartube-media-corpus/mpeg4_3ivx.avi | aee9369 shipping  |
| MISSING |  |  | 10 | audio:mp1 |  | ?ch 0k |  | gen:audio_mp1.mpa: missing file /Users/jd/projects/peartube-media-corpus/audio_mp1.mpa | aee9369 shipping  |
| MISSING |  |  | 10 | audio:qdmc |  | ?ch 0k |  | gen:audio_qdmc_placeholder.bin: missing file /Users/jd/projects/peartube-media-corpus/audi | 57e675d decided  |

verdicts: {'SLOW!': 1, 'SLOW': 8, 'FAIL': 20, 'ok': 81, 'ERROR': 12, 'MISSING': 3}
