# peartube-media

PearTube's media player. It replaces libVLC in the PearTube app (Android, macOS, iOS) with a Rust engine built on the [OxideAV](https://github.com/OxideAV) codec framework, plus the platforms' own video decoders.

## Goal

Play every format on VLC's published feature list (videolan.org/vlc/features.html) plus the modern ones VLC 3 plays (HEVC, VP8, VP9, AV1, Opus, WebVTT, PGS), from the HTTP stream URLs PearTube's worklet serves on 127.0.0.1. Discs, tuners, capture devices and network protocols other than HTTP are out of scope.

## Layout

|Path|Purpose|
|---|---|
|`crates/player`|The engine: source, demux, decoder choice, clock, sync, seek, tracks, and the platform backends (Android, Apple, headless)|
|`crates/codecs`|`register_all`: every OxideAV crate the player uses plus the `codec-*` crates here|
|`crates/codec-*`|Decoders and demuxers OxideAV lacks. Same shape as an OxideAV crate: implement `oxideav_core::Decoder` / `Demuxer`, export `register(&mut RuntimeContext)`|
|`crates/e2e`|Corpus runner: plays every corpus file through the headless backend and checks it against FFmpeg; writes `target/e2e/codecs.json`|

## Design

- **Source**: `oxideav-http`'s `HttpSource` (HTTP/1.1 Range, `Read + Seek`) behind a bounded read-ahead ring. Reads have a deadline, so a suspended worklet fails reads instead of hanging them; resume reopens at the last offset.
- **Demux**: `ContainerRegistry::probe_input` (rewinds after reading up to 256 KiB), then `open_demuxer(name, input, &codecs)`. One demux thread fills per-stream packet queues bounded in both duration and bytes.
- **Buffering**: the read-ahead ring reports when a read waits for bytes that have not arrived. The clock holds at the start until the first audio and video are decoded and about a second is queued (or the input ends), likewise after a seek, and mid-stream whenever a pipeline runs dry while the source is starved, until a second is queued past the clock again. `buffering` in the state follows the hold; `play`/`pause` stay the user's intent, so a pause during buffering stays paused when the data arrives.
- **Audio**: always decoded in software (OxideAV or `codec-*`) to PCM. Android plays it through AAudio, Apple through `AVSampleBufferAudioRenderer`. Audio is the master clock. On Android the clock comes from `AAudioStream_getTimestamp` once it is valid and from frames written before that.
- **Video**: the platform decoder first, chosen by trying it: Android `MediaCodec::from_decoder_type` + `configure` on the slot's `ANativeWindow` (the NDK has no codec-list API below API 36); Apple enqueues compressed `CMSampleBuffer`s on `AVSampleBufferDisplayLayer`. Any failure, at open or mid-stream, tears the platform decoder down and continues in software from the next keyframe. Software frames go to Android as RGBA_8888 through `ANativeWindow_lock` (after `oxideav-pixfmt` conversion), and to Apple as `CVPixelBuffer` sample buffers on the same layer.
- **Apple**: one `AVSampleBufferRenderSynchronizer` drives the display layer and the audio renderer. Layer work stays on the main thread, and a failed layer (`requiresFlushToResumeDecoding`) is flushed and re-fed from a keyframe.
- **Lifecycle**: `open` takes no surface; `set_surface` attaches or detaches one (Android `SurfaceView` callbacks, macOS/iOS view moves). `suspend` / `resume` follow the activity: stop audio, release the platform decoder and surface, resume at the last position.
- **Subtitles**: text, ASS and bitmap subtitles render to RGBA on an overlay above the video (a second `SurfaceView` on Android, a `CALayer` on Apple).
- **Untrusted input**: every stream comes from untrusted peers. Frame dimensions, stream count and queue bytes are capped, demux and decode run under `catch_unwind`, and the corpus includes truncated and mutated files.

## API used by the app

```rust
let player = Player::open(url, move |event| { /* Changed | Ended | Error(String) */ })?;
player.set_surface(Some(surface));
player.play(); player.pause(); player.seek(position);
player.select_audio(Some(id)); player.select_subtitle(None);
player.suspend(); player.resume();
let state = player.state(); // position, duration, playing, buffering, ended, error, tracks
```

## Dependencies on OxideAV

OxideAV crates are used at pinned git revisions; their crates.io releases lag their repositories. When a crate needs a fix, it is forked to `ayooooo123/oxideav-<name>` and `[patch.crates-io]` points the whole dependency graph at the fork. Fixes go upstream where OxideAV's clean-room rule allows.

## Licenses

Code in this repository is MIT unless a crate says otherwise. Decoders with no public specification (TrueHD/MLP, several Windows Media and RealMedia codecs) are ports of FFmpeg's LGPL-2.1-or-later decoders; each such crate is LGPL-2.1-or-later, carries its own LICENSE, and ports only FFmpeg files whose headers say LGPL.

## Verification

`cargo run -p e2e --release` plays the corpus (FFmpeg's FATE samples plus generated files) through the headless backend and compares every stream with FFmpeg: `framemd5` for bit-exact codecs, PSNR/SNR thresholds for the rest. Every format on the list needs a passing file. The result is `target/e2e/codecs.json`.

### AAC reference oracles

`cargo test -j 2 -p check-aac -- --nocapture` covers the AAC fork through
the production demuxers/decoder. All 27 legacy and eight unaffected USAC
FFmpeg floors are retained. Canonical `xhe_target_level.m4a` instead uses
the independently verified libxaac reference: FFmpeg 2da55bf does not
prime its AudioPreRoll and has a different first-channel noise seed.

Sixteen external ISO/IEC 23003-7 FD vectors exercise complex coefficients,
previous-frame prediction, short windows, TNS, STOP_START and noise filling.
Tests compare every presented sample with ISO PCM and every raw sample
with native libxaac, keeping MP4 edit-list trimming separate from decoding.
Per-channel floors are the measured baseline minus 0.5 dB. A separate
independent-window TNS equivalence fixture leaves every canonical input
unchanged. Setup, exact native commands, hashes and limits:
[ISO corpus](crates/check-aac/tests/data/iso-usac/README.md) and
[canonical xhe](crates/check-aac/tests/data/libxaac/README.md).

Release speed smoke on the development Mac: mono LC 300.2x, stereo LC
154.1x, HE-AAC v2 115.1x, 96 kHz six-channel LC 31.5x, canonical xhe
525.4x, and ISO 44.1 kHz window-switched complex prediction 211.5x real
time (`cargo run --release -p check-aac --example speed -- <paths>`).
These are codec measurements, not phone or full Player acceptance.
