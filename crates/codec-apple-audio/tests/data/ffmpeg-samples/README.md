# FFmpeg sample archive files

FATE has one QDM2 sample (`qt-surge-suite/surge-2-16-B-QDM2.mov`) and no
QDMC sample. The reference tests also decode these files from FFmpeg's own
sample archive, <https://samples.ffmpeg.org/>, kept under their archive
paths in `$FFMPEG_SAMPLES` (default `~/projects/oracles/ffmpeg-samples`).
They are external prerequisites, not part of this repository; the tests
fail with a pointer here when one is missing, they never skip it.
`SHA256SUMS` pins every file; each also matches the archive's own
`md5sum` list in its directory.

| File | Audio | Why |
|---|---|---|
| `A-codecs/QDM2/sweep/0-22050HzSweep{8,10,12,16,20,24,32,40,48,64}kb.mov` | QDM2, 44.1 kHz mono | one coding configuration per bitrate |
| `A-codecs/QDM2/sweep/0-2222050HzSweep24kbQT.mov` | QDM2, 44.1 kHz mono | QuickTime's own encode |
| `A-codecs/QDM2/fft8/resurrection.mov` | QDM2, 22.05 kHz mono | the archive's `fft8` case |
| `A-codecs/QDMC/rumcoke.mov`, `slick.mov` | QDMC, 44.1 kHz stereo | |
| `A-codecs/QDMC/tidemo1-24bit-rle.mov` | QDMC, 22.05 kHz mono | |

Fetch and check, from this crate's directory:

```sh
R=${FFMPEG_SAMPLES:-$HOME/projects/oracles/ffmpeg-samples}
cut -c67- tests/data/ffmpeg-samples/SHA256SUMS | while read -r f; do
  mkdir -p "$R/$(dirname "$f")"
  [ -s "$R/$f" ] || curl -sS --fail -o "$R/$f" "https://samples.ffmpeg.org/$f"
done
(cd "$R" && shasum -a 256 -c "$OLDPWD/tests/data/ffmpeg-samples/SHA256SUMS")
```
