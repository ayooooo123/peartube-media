#!/usr/bin/env bash
# corpus/generate.sh — makes the `gen:` corpus files with ffmpeg into
# PEARTUBE_CORPUS_DIR (default ~/projects/peartube-media-corpus, outside the
# repo). Each entry is ~5–10 s. Idempotent: every file is regenerated.
set -euo pipefail

CORPUS_DIR="${PEARTUBE_CORPUS_DIR:-$HOME/projects/peartube-media-corpus}"
mkdir -p "$CORPUS_DIR"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Generating synthetic test corpus in $CORPUS_DIR..."

# ---------------- shared inputs ----------------
V_IN=(-f lavfi -i "testsrc2=size=320x240:rate=25")
A_IN=(-f lavfi -i "sine=frequency=440:sample_rate=48000")
DUR=(-t 6)
VENC="-c:v libx264 -pix_fmt yuv420p -preset ultrafast"

# SRT / ASS / VTT subtitle sources
cat > "$TMP_DIR/sub.srt" <<'EOF'
1
00:00:00,500 --> 00:00:02,000
Hello from the corpus.

2
00:00:02,500 --> 00:00:04,000
Second cue.

3
00:00:04,500 --> 00:00:05,500
Third cue.
EOF
cat > "$TMP_DIR/sub.ass" <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 320
PlayResY: 240

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.50,0:00:02.00,Default,,0,0,0,,{\i1}ASS{\i0} cue one.
Dialogue: 0,0:00:02.50,0:00:04.00,Default,,0,0,0,,ASS cue two.
EOF
cat > "$TMP_DIR/sub.usf" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE USFSubtitles PUBLIC "-//USF//DTD Subtitles 1.0/EN" "http://ultravcs.sourceforge.net/usf/usf.dtd" []>
<USFSubtitles version="1.0">
  <metadata><title>corpus</title><date>2026</date></metadata>
  <styles><style name="Default"><fontstyle face="Arial" size="20"/></style></styles>
  <subtitles>
    <subtitle start="0.500" stop="2.000"><text>Hello from the USF corpus.</text></subtitle>
    <subtitle start="2.500" stop="4.000"><text>USF cue two.</text></subtitle>
    <subtitle start="4.500" stop="5.500"><text>USF cue three.</text></subtitle>
  </subtitles>
</USFSubtitles>
EOF

run() { ffmpeg -v error -y "$@" || echo "WARN: ffmpeg failed: $*" >&2; }

# ---------------- 1. H.264 + AC-3/E-AC-3/DTS/TrueHD/AAC in MKV, MP4, TS, AVI ----------------
for a in ac3:truehd:no eac3:no dts:no truehd:no aac:no; do :; done
for audio in ac3 eac3 dts truehd aac; do
  for m in mkv mp4 ts; do
    opts=(-strict -2)
    [ "$m" = mp4 ] && [ "$audio" = truehd ] && opts=(-tag:a mlpa -strict -2)
    # truehd in mp4 needs mlpa tag; AVI cannot carry truehd
    run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" $VENC -c:a "$audio" "${opts[@]}" \
      "$CORPUS_DIR/h264_${audio}.${m}"
  done
  if [ "$audio" != truehd ]; then
    run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" $VENC -c:a "$audio" -strict -2 \
      "$CORPUS_DIR/h264_${audio}.avi"
  fi
done

# ---------------- 2. HEVC 10-bit + E-AC-3 in MKV ----------------
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v libx265 -pix_fmt yuv420p10le -preset ultrafast \
  -c:a eac3 -strict -2 "$CORPUS_DIR/hevc10_eac3.mkv"

# ---------------- 3. VP9 + Opus in WebM ----------------
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v libvpx-vp9 -pix_fmt yuv420p \
  -c:a libopus "$CORPUS_DIR/vp9_opus.webm"

# ---------------- 4. AV1 + Opus in MKV ----------------
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v libsvtav1 -pix_fmt yuv420p \
  -c:a libopus "$CORPUS_DIR/av1_opus.mkv"

# ---------------- 5. MPEG-2 + MP2 in MPEG-PS; MPEG-1 in PS ----------------
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v mpeg2video -pix_fmt yuv420p -q:v 8 \
  -c:a mp2 "$CORPUS_DIR/mpeg2_mp2.mpg"
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v mpeg1video -pix_fmt yuv420p -q:v 8 \
  -c:a mp2 "$CORPUS_DIR/mpeg1_mp2.mpg"

# ---------------- 6. Subtitle muxes ----------------
run "${V_IN[@]}" "${A_IN[@]}" -i "$TMP_DIR/sub.srt" "${DUR[@]}" $VENC -c:a aac -c:s srt \
  "$CORPUS_DIR/h264_aac_srt.mkv"
run "${V_IN[@]}" "${A_IN[@]}" -i "$TMP_DIR/sub.ass" "${DUR[@]}" $VENC -c:a aac -c:s ass \
  "$CORPUS_DIR/h264_aac_ass.mkv"
run "${V_IN[@]}" "${A_IN[@]}" -i "$TMP_DIR/sub.srt" "${DUR[@]}" -c:v libvpx-vp9 -pix_fmt yuv420p \
  -c:a libopus -c:s webvtt "$CORPUS_DIR/vp9_opus_vtt.webm"
run "${V_IN[@]}" "${A_IN[@]}" -i "$TMP_DIR/sub.srt" "${DUR[@]}" $VENC -c:a aac -c:s mov_text \
  "$CORPUS_DIR/h264_aac_movtext.mp4"
# QuickTime: FFmpeg writes mov_text into MOV as a `text` entry; `-tag:s tx3g`
# keeps the 3GPP entry MP4 uses.
run "${V_IN[@]}" "${A_IN[@]}" -i "$TMP_DIR/sub.srt" "${DUR[@]}" $VENC -c:a aac -c:s mov_text \
  "$CORPUS_DIR/h264_aac_movtext.mov"
run "${V_IN[@]}" "${A_IN[@]}" -i "$TMP_DIR/sub.srt" "${DUR[@]}" $VENC -c:a aac -c:s mov_text -tag:s tx3g \
  "$CORPUS_DIR/h264_aac_tx3g.mov"
# PGS into MKV: ffmpeg cannot encode PGS; remux FATE's .sup when present.
if [ -f "$HOME/projects/fate-suite/sub/pgs_sub.sup" ]; then
  run -f lavfi -i "testsrc2=size=320x240:rate=25:duration=6" -i "$HOME/projects/fate-suite/sub/pgs_sub.sup" \
    $VENC -c:v libx264 -c:s copy "$CORPUS_DIR/h264_aac_pgs.mkv"
fi
# USF is plain XML in MKV's S_USF… ffmpeg has no USF encoder; a USF-in-MKV is
# an attachment the subs worker muxes by hand. Copy the XML for the runner to
# probe-demux when that lands; until then this file exercises probe rejection.
cp "$TMP_DIR/sub.usf" "$CORPUS_DIR/sub.usf"

# ---------------- 7. Standalone formats ----------------
run "${A_IN[@]}" "${DUR[@]}" -c:a flac "$CORPUS_DIR/audio.flac"
run "${A_IN[@]}" "${DUR[@]}" -c:a alac "$CORPUS_DIR/audio_alac.m4a"
run "${A_IN[@]}" "${DUR[@]}" -c:a libvorbis "$CORPUS_DIR/audio_vorbis.ogg"
# Raw containers: ADTS AAC and Opus in an .opus file.
run "${A_IN[@]}" "${DUR[@]}" -c:a aac -f adts "$CORPUS_DIR/audio.aac"
run "${A_IN[@]}" "${DUR[@]}" -c:a libopus -b:a 128k "$CORPUS_DIR/audio.opus"
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v libtheora -pix_fmt yuv420p -c:a libvorbis \
  "$CORPUS_DIR/video_theora.ogg"
run -f lavfi -i "sine=frequency=440:sample_rate=8000" "${DUR[@]}" -c:a pcm_alaw "$CORPUS_DIR/audio_alaw.wav"
run -f lavfi -i "sine=frequency=440:sample_rate=8000" "${DUR[@]}" -c:a pcm_mulaw "$CORPUS_DIR/audio_ulaw.wav"
run "${A_IN[@]}" "${DUR[@]}" -c:a pcm_s16le "$CORPUS_DIR/audio_lpcm.wav"
run -f lavfi -i "sine=frequency=440:sample_rate=44100" "${DUR[@]}" -c:a adpcm_ms "$CORPUS_DIR/audio_adpcm.wav"
run -f lavfi -i "sine=frequency=440:sample_rate=8000" "${DUR[@]}" -c:a g726 -b:a 24k "$CORPUS_DIR/audio_g726.wav"
# Blu-ray LPCM (stream type 0x80 under the HDMV registration) in M2TS.
run "${A_IN[@]}" "${DUR[@]}" -c:a pcm_bluray -sample_fmt s16 -ac 2 -mpegts_m2ts_mode 1 -f mpegts \
  "$CORPUS_DIR/audio_pcm_bluray.m2ts"
# Raw DV (PAL 4:2:0, 48 kHz stereo PCM) as FFmpeg's dv muxer writes it.
run -f lavfi -i "testsrc2=size=720x576:rate=25" "${A_IN[@]}" "${DUR[@]}" -c:v dvvideo -pix_fmt yuv420p \
  -c:a pcm_s16le -ac 2 -f dv "$CORPUS_DIR/video.dv"
run "${A_IN[@]}" "${DUR[@]}" -c:a libmp3lame -b:a 128k "$CORPUS_DIR/audio.mp3"
# Genuine WMV1/WMA1, rather than attributing the WMV2/WMA2 FATE clip to both.
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v wmv1 -q:v 10 -c:a wmav1 -b:a 128k \
  "$CORPUS_DIR/wmv1_wma1.asf"
run "${V_IN[@]}" "${DUR[@]}" -c:v mjpeg -pix_fmt yuvj420p -q:v 6 "$CORPUS_DIR/video_mjpeg.avi"
run "${V_IN[@]}" "${DUR[@]}" -c:v mpeg4 -pix_fmt yuv420p -q:v 6 "$CORPUS_DIR/video_mpeg4.avi"
run "${V_IN[@]}" "${DUR[@]}" -c:v mpeg4 -pix_fmt yuv420p -q:v 6 -vtag XVID "$CORPUS_DIR/mpeg4_xvid.avi"
run -f lavfi -i "testsrc2=size=176x144:rate=15" "${DUR[@]}" -c:v h261 -pix_fmt yuv420p "$CORPUS_DIR/video_h261.avi"
run -f lavfi -i "testsrc2=size=176x144:rate=15" "${DUR[@]}" -c:v h263 -pix_fmt yuv420p "$CORPUS_DIR/video_h263.avi"
# H.263i = H.263 with Intel annex differences; ffmpeg encodes h263 and tags it
# as h263i is impossible, so exercise the same stream tagged H.263 (the
# decoder must handle the base profile either way).
ffmpeg -v error -y -f lavfi -i "testsrc2=size=176x144:rate=15" -t 6 -c:v h263 -pix_fmt yuv420p "$CORPUS_DIR/video_h263i.avi"
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v libvpx -pix_fmt yuv420p -c:a libvorbis \
  "$CORPUS_DIR/video_vp8.webm"
run "${A_IN[@]}" "${DUR[@]}" -c:a pcm_s16be "$CORPUS_DIR/audio.aiff"
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" -c:v flv1 -pix_fmt yuv420p -c:a libmp3lame \
  "$CORPUS_DIR/video.flv"
run "${V_IN[@]}" "${A_IN[@]}" "${DUR[@]}" $VENC -c:a aac -f nut "$CORPUS_DIR/h264_aac_nut.nut"

# ---------------- 8. Hand-rolled containers ffmpeg cannot write ----------------
# 8a. ProTracker 4-channel MOD (31 samples, 1 pattern, "M.K.") for audio:mod.
#     codec-tracker loads and renders it; libopenmpt is the audio oracle.
python3 - "$CORPUS_DIR/audio_mod.mod" <<'PYEOF'
import struct, sys
out = bytearray()
out += b"e2e corpus".ljust(20, b"\0")          # title

def sample_header(name, length_words):
    h = bytearray()
    h += name.ljust(22, b"\0")
    h += bytes([0, 40])                          # finetune 0, volume 40
    h += struct.pack(">H", length_words)         # length in words
    h += bytes([0, 0, 0, 0])                     # repeat point/length
    return bytes(h)
for i in range(31):
    length = 64 if i == 0 else 0
    out += sample_header(f"smp{i}".encode(), length)
out += bytes([1, 0])                            # song length 1, restart 0
out += bytes(128)                               # 128-byte pattern order table (1 position)
out += b"M.K."                                  # 4-channel ProTracker at offset 1080
# one 64-row pattern, all zeros with a note on row 0 of channel 0
pattern = bytearray(64 * 4 * 4)
# period 428 (C-3), sample 1, effect C40 (set volume 64) on row 0 ch0
pattern[0:4] = bytes([0x14, 0xAC, 0x10, 0xC4])
out += pattern
out += bytes([64])                              # sample 1 data: 64*2 words of square-ish wave
wave = bytearray()
for i in range(64):
    v = 64 if (i // 8) % 2 == 0 else -64
    wave += struct.pack(">b", v)
out += wave
open(sys.argv[1], "wb").write(bytes(out))
PYEOF

# 8b. Standard MIDI File format 0 (one track, a few notes) for audio:midi /
#     container:midi. FFmpeg 9 cannot write SMF.
python3 - "$CORPUS_DIR/audio.mid" <<'PYEOF'
import struct, sys
def vlq(n):
    b = [n & 0x7F]
    n >>= 7
    while n:
        b.append((n & 0x7F) | 0x80)
        n >>= 7
    return bytes(reversed(b))
track = bytearray()
def ev(dt, data):
    track.extend(vlq(dt)); track.extend(data)
# tempo 120bpm
ev(0, b"\xff\x51\x03" + (500000).to_bytes(3, "big"))
ev(0, b"\xff\x58\x04" + bytes([4, 2, 24, 8]))   # 4/4
for i, note in enumerate([60, 64, 67, 72]):
    t = 0 if i == 0 else 240
    ev(t, bytes([0x90, note, 100]))
    ev(240, bytes([0x80, note, 64]))
ev(0, b"\xff\x2f\x00")
smf = b"MThd" + struct.pack(">IHHH", 6, 0, 1, 96) + b"MTrk" + struct.pack(">I", len(track)) + bytes(track)
open(sys.argv[1], "wb").write(smf)
PYEOF

# 8d. CMML in Ogg (https://wiki.xiph.org/CMML): the ident header (granule
#     rate 1000/1, shift 32), the preamble and head headers, one clip per
#     page at granule (time << 32 | previous clip), an empty clip on the EOS
#     page. FFmpeg cannot write or read CMML.
python3 - "$CORPUS_DIR/sub_cmml.ogg" <<'PYEOF'
import struct, sys
TABLE = []
for i in range(256):
    r = i << 24
    for _ in range(8):
        r = ((r << 1) ^ 0x04C11DB7) if r & 0x80000000 else (r << 1)
        r &= 0xFFFFFFFF
    TABLE.append(r)
def crc(data):
    c = 0
    for b in data:
        c = ((c << 8) & 0xFFFFFFFF) ^ TABLE[((c >> 24) ^ b) & 0xFF]
    return c
def page(flags, granule, seq, packets):
    lacing = b''.join(b'\xff' * (len(p) // 255) + bytes([len(p) % 255]) for p in packets)
    head = b'OggS' + bytes([0, flags]) + struct.pack('<qII', granule, 0x434D4D4C, seq)
    tail = bytes([len(lacing)]) + lacing + b''.join(packets)
    return head + struct.pack('<I', crc(head + b'\0\0\0\0' + tail)) + tail
ident = b'CMML\0\0\0\0' + struct.pack('<HHqqB', 2, 1, 1000, 1, 32)
preamble = (b'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
            b'<!DOCTYPE cmml SYSTEM "cmml.dtd">\n<?cmml lang="en"?>')
head = b'<head>\n<title>PearTube corpus</title>\n</head>'
clips = [(1000, b'<clip id="one" track="main"><desc>First clip</desc></clip>'),
         (3000, b'<clip id="two" track="main"><desc>Second clip</desc></clip>'),
         (5000, b'<clip track="main"/>')]
out = page(2, 0, 0, [ident]) + page(0, 0, 1, [preamble, head])
previous = 0
for i, (ms, clip) in enumerate(clips):
    out += page(4 if i == len(clips) - 1 else 0, (ms << 32) | previous, i + 2, [clip])
    previous = ms
open(sys.argv[1], 'wb').write(out)
PYEOF

# 8e. MPEG-1 Layer I (.mpa) for audio:mp1 / container:mp3: FFmpeg has no MP1
#     encoder. Written in the subband domain (no analysis filter): a 440 Hz
#     tone on the left (subband 0), a tone in subband 1 on the right, and
#     quieter shared tones in subbands 5-20, louder on the left. 44.1 kHz,
#     384 kbit/s with the padding rule, joint stereo with the mode extension
#     stepping through its four bounds every frame, CRC words over the header
#     and the bit allocation as ISO 11172-3 has them (FFmpeg's crccheck, off
#     by default, covers 256 bits for any two-channel frame and so reports
#     joint stereo frames as mismatches). 689 frames, 6 s.
python3 - "$CORPUS_DIR/audio_mp1.mpa" <<'PYEOF'
import math, sys
RATE, BITRATE, FRAMES = 44100, 384000, 689
SUBBAND_RATE = RATE / 32
def crc16(bits):
    crc = 0xFFFF
    for b in bits:
        top = (crc >> 15) & 1
        crc = (crc << 1) & 0xFFFF
        if top != b:
            crc ^= 0x8005
    return crc
def scf_index(peak):
    # The smallest Layer I scale factor 2 * 2^(-i/3) at or above peak.
    return max(0, min(62, int(math.floor(3 * math.log2(2.0 / peak)))))
def scf(i):
    return 2.0 * 2.0 ** (-i / 3)
def quant(x, nb):
    half = (1 << (nb - 1)) - 1
    return max(0, min((1 << nb) - 2, int(round(x * half)) + half))
def tone(freq, n, phase=0.0):
    return math.cos(2 * math.pi * freq * n / SUBBAND_RATE + phase)
out = bytearray()
rest = 0
for f in range(FRAMES):
    rest += BITRATE * 12 % RATE
    pad = 1 if rest >= RATE else 0
    rest -= RATE * pad
    size = (BITRATE * 12 // RATE + pad) * 4
    mode_ext = f % 4
    bound = (mode_ext + 1) * 4
    header = 0xFFF00000 | 1 << 19 | 3 << 17 | 12 << 12 | pad << 9 | 1 << 6 | mode_ext << 4 | 1 << 2
    n0 = 12 * f
    # Per band: the allocation (mantissa bits - 1) and each channel's
    # samples, or one shared normalized signal and two amplitudes.
    alloc = [[0] * 32, [0] * 32]
    samples = [[None] * 32, [None] * 32]
    alloc[0][0] = 14
    samples[0][0] = [0.5 * tone(440, n0 + j) for j in range(12)]
    alloc[1][1] = 14
    samples[1][1] = [0.4 * tone(300, n0 + j) for j in range(12)]
    shared = {}
    for sb in range(5, 21):
        s = [tone(37 + 11 * sb, n0 + j, sb) for j in range(12)]
        shared[sb] = s
        for ch, amp in ((0, 0.03), (1, 0.015)):
            alloc[ch][sb] = 5
            samples[ch][sb] = [amp * v for v in s]
    bits = []
    def put(v, n):
        bits.extend((v >> i) & 1 for i in range(n - 1, -1, -1))
    put(header, 32)
    put(0, 16)
    for sb in range(bound):
        for ch in range(2):
            put(alloc[ch][sb], 4)
    for sb in range(bound, 32):
        put(alloc[0][sb], 4)
    crc = crc16(bits[16:32] + bits[48:])
    bits[32:48] = [(crc >> i) & 1 for i in range(15, -1, -1)]
    # Scale factors and normalized samples; above the bound one set of
    # samples (the shared signal) with a scale factor per channel.
    sfs = [[0] * 32, [0] * 32]
    norm = [[None] * 32, [None] * 32]
    for sb in range(32):
        for ch in range(2):
            if alloc[ch][sb]:
                peak = max(abs(v) for v in samples[ch][sb]) or 1e-9
                sfs[ch][sb] = scf_index(peak)
                norm[ch][sb] = [v / scf(sfs[ch][sb]) for v in samples[ch][sb]]
        if sb >= bound and alloc[0][sb]:
            peak = max(abs(v) for v in shared[sb])
            norm[0][sb] = [v / peak for v in shared[sb]]
            for ch, amp in ((0, 0.03), (1, 0.015)):
                sfs[ch][sb] = scf_index(amp * peak)
    for sb in range(32):
        for ch in range(2):
            if alloc[0 if sb >= bound else ch][sb]:
                put(sfs[ch][sb], 6)
    for j in range(12):
        for sb in range(bound):
            for ch in range(2):
                if alloc[ch][sb]:
                    put(quant(norm[ch][sb][j], alloc[ch][sb] + 1), alloc[ch][sb] + 1)
        for sb in range(bound, 32):
            if alloc[0][sb]:
                put(quant(norm[0][sb][j], alloc[0][sb] + 1), alloc[0][sb] + 1)
    assert len(bits) <= size * 8, "frame overflow"
    bits += [0] * (size * 8 - len(bits))
    out += int("".join(map(str, bits)), 2).to_bytes(size, "big")
open(sys.argv[1], "wb").write(bytes(out))
PYEOF

# 8c. Copy the USF alongside (done above). Done.
echo "Done! Corpus in $CORPUS_DIR ($(ls "$CORPUS_DIR" | wc -l | tr -d ' ') files)."
