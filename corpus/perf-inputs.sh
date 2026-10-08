#!/usr/bin/env bash
# corpus/perf-inputs.sh — makes the standard decode-speed inputs that
# `cargo run --release -p perf` measures, into $PEARTUBE_CORPUS_DIR/perf
# (default ~/projects/peartube-media-corpus/perf): 10–30 s each, about
# 0.45 GB in total. Idempotent: every file is regenerated.
#
# Sources come from the FATE suite ($FATE_SUITE, default ~/projects/fate-suite):
#   video  Big Buck Bunny 854x480p30 (mov/buck480p30_na.mp4), 30 s from 60 s,
#          upscaled with Lanczos plus light temporal luma grain so the encoders
#          spend their bitrate the way they do on camera footage;
#   audio  a 79 s stereo AAC programme (h264/unescaped_extradata.mp4); 5.1
#          takes three different 30 s stretches of it, one per channel pair;
#   FLAC 24/96  the divertimento in audio-reference/.
# FFmpeg has no encoder for E-AC-3 7.1, TrueHD 7.1 or WMA Pro: those repeat a
# FATE sample to length (raw sync frames concatenate; WMA Pro is remuxed).
set -euo pipefail

FATE="${FATE_SUITE:-$HOME/projects/fate-suite}"
OUT="${PEARTUBE_CORPUS_DIR:-$HOME/projects/peartube-media-corpus}/perf"
mkdir -p "$OUT"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

ff() { ffmpeg -hide_banner -nostdin -v error -y "$@"; }

# Optional group for regenerating HD, SD, audio, legacy gap or new-decoder inputs.
GROUP="${1:-all}"
case "$GROUP" in all|hd|sd|audio|legacy|new) ;; *) echo "usage: $0 [all|hd|sd|audio|legacy|new]" >&2; exit 2 ;; esac

BBB="$FATE/mov/buck480p30_na.mp4"
MUSIC="$FATE/h264/unescaped_extradata.mp4"
DIVERTIMENTO="$FATE/audio-reference/divertimenti_2ch_96kHz_s24.wav"
for f in "$BBB" "$MUSIC" "$DIVERTIMENTO" "$FATE/eac3/the_great_wall_7.1.eac3" \
  "$FATE/truehd/atmos.thd" "$FATE/wmapro/latin_192_mulitchannel_cut.wma"; do
  [ -f "$f" ] || { echo "missing FATE sample $f" >&2; exit 1; }
done

GRAIN="noise=c0s=4:c0f=t"
SRC=(-ss 60 -t 30 -i "$BBB" -an)

# ---------------- video ----------------
if [[ "$GROUP" == all || "$GROUP" == hd ]]; then
echo "H.264 High 1080p30"
ff "${SRC[@]}" -vf "scale=1920:1080:flags=lanczos,$GRAIN" -c:v libx264 -preset medium \
  -profile:v high -level 4.1 -pix_fmt yuv420p -b:v 8M -maxrate 10M -bufsize 16M \
  "$OUT/h264_1080p30_high.mp4"

echo "HEVC Main / Main10 1080p30"
ff "${SRC[@]}" -vf "scale=1920:1080:flags=lanczos,$GRAIN" -c:v libx265 -preset medium \
  -profile:v main -pix_fmt yuv420p -b:v 5M -x265-params log-level=error \
  "$OUT/hevc_1080p30_main.mkv"
ff "${SRC[@]}" -vf "scale=1920:1080:flags=lanczos,$GRAIN" -c:v libx265 -preset medium \
  -profile:v main10 -pix_fmt yuv420p10le -b:v 5M -x265-params log-level=error \
  "$OUT/hevc_1080p30_main10.mkv"

echo "VP9 1080p30"
ff "${SRC[@]}" -vf "scale=1920:1080:flags=lanczos,$GRAIN" -c:v libvpx-vp9 -b:v 5M \
  -deadline good -cpu-used 4 -row-mt 1 -tile-columns 2 -threads 8 \
  "$OUT/vp9_1080p30.webm"

echo "AV1 1080p30"
ff "${SRC[@]}" -vf "scale=1920:1080:flags=lanczos,$GRAIN" -c:v libsvtav1 -preset 8 \
  -b:v 4M -pix_fmt yuv420p "$OUT/av1_1080p30.mkv"

echo "MPEG-2 720p30 (transport stream)"
ff -ss 60 -t 20 -i "$BBB" -an -vf "scale=1280:720:flags=lanczos,$GRAIN" -c:v mpeg2video \
  -b:v 15M -maxrate 18M -bufsize 7340032 -g 15 -bf 2 "$OUT/mpeg2_720p30.ts"
fi

if [[ "$GROUP" == all || "$GROUP" == sd ]]; then
echo "MPEG-2 576i25 (elementary stream)"
ff "${SRC[@]}" -vf "fps=50,scale=720:576:flags=lanczos,$GRAIN,interlace=scan=tff:lowpass=complex" \
  -c:v mpeg2video -flags +ilme+ildct -b:v 6M -maxrate 9.8M -bufsize 1835008 -g 12 -bf 2 \
  -aspect 16:9 -f mpeg2video "$OUT/mpeg2_576i25.m2v"

echo "DV 576i25"
ff -ss 60 -t 20 -i "$BBB" -an -vf "fps=50,scale=720:576:flags=lanczos,$GRAIN,interlace=scan=bff" \
  -c:v dvvideo -pix_fmt yuv420p -aspect 16:9 "$OUT/dv_576i25.avi"

echo "MPEG-4 ASP (Xvid) 480p30"
ff "${SRC[@]}" -vf "scale=848:480:flags=lanczos,$GRAIN" -c:v mpeg4 -vtag XVID -b:v 1500k \
  -bf 2 -flags +mv4+aic -mbd rd -trellis 1 -g 300 "$OUT/mpeg4_asp_480p30.avi"
fi

# ---------------- audio ----------------
if [[ "$GROUP" == all || "$GROUP" == audio ]]; then
# Stereo 30 s at 48 kHz and 44.1 kHz, and 5.1(side) at 48 kHz from three
# stretches of the programme (FL/FR, FC/LFE, SL/SR).
ff -ss 20 -t 30 -i "$MUSIC" -vn -ac 2 -ar 48000 -c:a pcm_s16le "$TMP/stereo48.wav"
ff -ss 20 -t 30 -i "$MUSIC" -vn -ac 2 -ar 44100 -c:a pcm_s16le "$TMP/stereo44.wav"
ff -i "$MUSIC" -vn -filter_complex \
  "[0:a]aresample=48000,aformat=sample_fmts=s16:channel_layouts=stereo,asplit=3[a][b][c];\
[a]atrim=0:30,asetpts=PTS-STARTPTS[s0];[b]atrim=24:54,asetpts=PTS-STARTPTS[s1];\
[c]atrim=48:78,asetpts=PTS-STARTPTS[s2];\
[s0][s1][s2]amerge=inputs=3,pan=5.1(side)|FL=c0|FR=c1|FC=c2|LFE=c3|SL=c4|SR=c5[out]" \
  -map "[out]" -c:a pcm_s16le "$TMP/surround48.wav"

echo "AAC-LC stereo / 5.1, HE-AAC v1 / v2"
ff -i "$TMP/stereo48.wav" -c:a aac -b:a 160k "$OUT/aac_lc_stereo.m4a"
ff -i "$TMP/surround48.wav" -c:a aac -b:a 384k "$OUT/aac_lc_51.m4a"
ff -i "$TMP/stereo48.wav" -c:a aac_at -profile:a 4 -b:a 64k "$OUT/he_aac_stereo.m4a"
ff -i "$TMP/stereo48.wav" -c:a aac_at -profile:a 28 -b:a 32k "$OUT/he_aac_v2_stereo.m4a"

echo "AC-3 5.1, DTS 5.1"
ff -i "$TMP/surround48.wav" -c:a ac3 -b:a 448k "$OUT/ac3_51.ac3"
# FFmpeg 9's arm64 optimized DCA encoder crashes on this six-channel input.
ff -cpuflags 0 -i "$TMP/surround48.wav" -c:a dca -strict -2 -b:a 1509k "$OUT/dts_51.dts"

echo "E-AC-3 7.1, TrueHD 7.1 (FATE samples repeated)"
: > "$OUT/eac3_71.eac3"
for _ in 1 2 3 4 5; do cat "$FATE/eac3/the_great_wall_7.1.eac3" >> "$OUT/eac3_71.eac3"; done
: > "$OUT/truehd_71.thd"
for _ in $(seq 250); do cat "$FATE/truehd/atmos.thd" >> "$OUT/truehd_71.thd"; done

echo "FLAC 24/96"
ff -stream_loop 2 -i "$DIVERTIMENTO" -t 30 -af aresample=96000 -c:a flac -sample_fmt s32 \
  -bits_per_raw_sample 24 "$OUT/flac_24_96.flac"

echo "Opus, Vorbis, MP3"
ff -i "$TMP/stereo48.wav" -c:a libopus -b:a 128k "$OUT/opus_stereo.opus"
ff -i "$TMP/stereo44.wav" -c:a libvorbis -q:a 5 "$OUT/vorbis_stereo.ogg"
ff -i "$TMP/stereo44.wav" -c:a libmp3lame -b:a 192k "$OUT/mp3_stereo.mp3"

echo "WMA Pro 5.1 (FATE sample remuxed six times)"
for _ in 1 2 3 4 5 6; do
  printf "file '%s'\noutpoint 4.25\n" "$FATE/wmapro/latin_192_mulitchannel_cut.wma"
done > "$TMP/wmapro.txt"
ff -f concat -safe 0 -i "$TMP/wmapro.txt" -c copy "$OUT/wmapro_51.wma"
fi

# Registered decoders whose manifest sample is absent, mislabeled, or
# inaccessible through the player's demuxer. Keep original failures in the
# report; these separate inputs measure the codec rather than bypass routing.
if [[ "$GROUP" == all || "$GROUP" == legacy ]]; then
echo "Legacy gap inputs: MPEG-1, WMV1, WMA1, 3ivx, SVQ1/3, Dirac"
ff -ss 60 -t 20 -i "$BBB" -an -vf scale=848:480 -c:v mpeg1video -b:v 2M -bf 2 "$OUT/mpeg1_480p30.m1v"
ff -ss 60 -t 20 -i "$BBB" -an -vf scale=848:480 -c:v wmv1 -b:v 1500k "$OUT/wmv1_480p30.avi"
ff -ss 20 -t 30 -i "$MUSIC" -vn -ac 2 -ar 44100 -c:a wmav1 -b:a 160k "$OUT/wma1_stereo.wma"
# Tag coverage, not a claim that FFmpeg is the proprietary 3ivx encoder.
ff -ss 60 -t 20 -i "$BBB" -an -vf scale=848:480 -c:v mpeg4 -vtag 3IV2 -b:v 1500k "$OUT/mpeg4_3ivx.avi"
ff -i "$FATE/svq1/marymary-shackles.mov" -t 20 -map 0:v:0 -c copy "$OUT/svq1_rewrapped.mov"
ff -i "$FATE/svq3/Vertical400kbit.sorenson3.mov" -t 20 -map 0:v:0 -c copy "$OUT/svq3_rewrapped.mov"
ff -i "$FATE/dirac/vts.profile-main.drc" -map 0:v:0 -c copy "$OUT/dirac_rewrapped.mkv"
fi

# Decoders added since the first audit. Speex, AMR, ATRAC3+, QDM2, MACE and
# VP3-VP6 have no encoder in FFmpeg: their manifest samples are measured.
# ATRAC3 has no input here: its FATE samples are WAV, whose extradata the
# WAV demuxer drops until the AviWav branch merges, and an OMA rewrap with
# `-c copy` does not decode in FFmpeg either. The MXF file's PCM stream
# times the MXF demuxer: decoding PCM costs next to nothing. Raw .mp2 is kept
# to show its routing.
if [[ "$GROUP" == all || "$GROUP" == new ]]; then
echo "H.263 CIF and 4CIF, Sorenson H.263 (FLV) 480p, DVCPRO HD 1080i50"
ff -ss 60 -t 20 -i "$BBB" -an -vf "scale=352:288:flags=lanczos,$GRAIN" -c:v h263 -b:v 768k -g 300 "$OUT/h263_cif.avi"
ff -ss 60 -t 20 -i "$BBB" -an -vf "scale=704:576:flags=lanczos,$GRAIN" -c:v h263 -b:v 2M -g 300 "$OUT/h263_4cif.avi"
ff -ss 60 -t 20 -i "$BBB" -an -vf "scale=848:480:flags=lanczos,$GRAIN" -c:v flv -b:v 1500k -g 300 "$OUT/flv1_480p30.flv"
ff -ss 60 -t 10 -i "$BBB" -an -vf "fps=25,scale=1440:1080:flags=lanczos,$GRAIN" -pix_fmt yuv422p \
  -c:v dvvideo -f mov "$OUT/dvcprohd_1080i50.mov"

echo "MP2 stereo (raw and Matroska), ALAC stereo, MXF (MPEG-2 + PCM)"
ff -ss 20 -t 30 -i "$MUSIC" -vn -ac 2 -ar 48000 -c:a mp2 -b:a 256k "$OUT/mp2_stereo.mp2"
ff -i "$OUT/mp2_stereo.mp2" -c copy "$OUT/mp2_stereo.mka"
ff -ss 20 -t 30 -i "$MUSIC" -vn -ac 2 -ar 44100 -c:a alac "$OUT/alac_stereo.m4a"
ff -ss 60 -t 20 -i "$BBB" -ss 20 -t 20 -i "$MUSIC" -map 0:v -map 1:a \
  -vf "scale=720:576:flags=lanczos" -r 25 -c:v mpeg2video -b:v 8M -g 12 \
  -c:a pcm_s16le -ar 48000 -ac 2 -f mxf "$OUT/mpeg2_pcm.mxf"

echo "DV audio: Ulead WAV (tag 0x0216) of the audio DIF blocks of 20 s of PAL DV"
ff -ss 60 -t 20 -i "$BBB" -ss 20 -t 20 -i "$MUSIC" -map 0:v -map 1:a -vf "scale=720:576,fps=25" \
  -pix_fmt yuv420p -c:v dvvideo -c:a pcm_s16le -ar 48000 -ac 2 -f dv "$TMP/pal.dv"
python3 - "$TMP/pal.dv" "$OUT/dvaudio_ulead.wav" <<'EOF'
# The layout crates/codec-dv/tests/dvaudio.rs builds: per DV frame, the nine
# audio DIF blocks (6, 22, ..., 134) of each of the 12 PAL DIF sequences.
import struct, sys
dv = open(sys.argv[1], 'rb').read()
data = bytearray()
for f in range(len(dv) // 144000):
    frame = dv[f * 144000:(f + 1) * 144000]
    for seq in range(12):
        for blk in range(9):
            at = seq * 150 * 80 + (6 + 16 * blk) * 80
            data += frame[at:at + 80]
align = 12 * 9 * 80
fmt = struct.pack('<HHIIHH', 0x0216, 2, 48000, align * 25, align, 16)
body = b'WAVE' + b'fmt ' + struct.pack('<I', len(fmt)) + fmt + b'data' + struct.pack('<I', len(data)) + bytes(data)
open(sys.argv[2], 'wb').write(b'RIFF' + struct.pack('<I', len(body)) + body)
EOF
fi

du -sh "$OUT"
