# FFmpeg sample archive files

FATE has no Intel H.263 sample and no H.263 file from a real encoder: its
H.263 tests encode their own streams (see `tests/reference.rs`). The
reference tests also decode these files from FFmpeg's own sample archive,
<https://samples.ffmpeg.org/>, kept under their archive paths in
`$FFMPEG_SAMPLES` (default `~/projects/oracles/ffmpeg-samples`). They are
external prerequisites, not part of this repository; the tests fail with a
pointer here when one is missing, they never skip it. `SHA256SUMS` pins
every file; each also matches the archive's own `md5sum` list in its
directory.

| File | Video | Why |
|---|---|---|
| `V-codecs/I263/i263.avi` | Intel H.263, 352x240 | loop filter, custom size from the container, dummy frames; MP3 stored by the byte (`strh.dwSampleSize` 1), played in `tests/player.rs` |
| `V-codecs/I263/i263_2.avi` | Intel H.263, 320x240 | the same with long vectors; 16-bit PCM |
| `V-codecs/h263/{baikonur_r7_overflight,baikonur_r7_rollout,iss_soyuztm32_launch,pooch,100374}.mov` | H.263 CIF in QuickTime | long vectors, a real encoder's streams |

The archive's raw `V-codecs/h263/h263-raw/messenger.h263` is left out: it
is a buffer dump (pictures in 32 KiB slots padded with `0xcd`), and
FFmpeg reports two of its pictures damaged and conceals them.

Fetch and check, from this crate's directory:

```sh
R=${FFMPEG_SAMPLES:-$HOME/projects/oracles/ffmpeg-samples}
cut -c67- tests/data/ffmpeg-samples/SHA256SUMS | while read -r f; do
  mkdir -p "$R/$(dirname "$f")"
  [ -s "$R/$f" ] || curl -sS --fail -o "$R/$f" "https://samples.ffmpeg.org/$f"
done
(cd "$R" && shasum -a 256 -c "$OLDPWD/tests/data/ffmpeg-samples/SHA256SUMS")
```

The FATE streams are regenerated on first use from `$FFMPEG_SRC` (FFmpeg
2da55bf): its `tests/videogen.c` and `tests/rotozoom.c` built with the C
compiler on PATH, then its `ffmpeg` with `fate-run.sh`'s command. Each
must equal the stream MD5 in `tests/ref/vsynth/`.
