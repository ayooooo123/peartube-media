# peartube-media

PearTube's media player. It replaces libVLC in the PearTube app (Android, macOS, iOS) with a Rust engine built on the [OxideAV](https://github.com/OxideAV) codec framework, plus the platforms' own video decoders.

## Goal

Play every format on VLC's published feature list (videolan.org/vlc/features.html) plus the modern ones VLC 3 plays (HEVC, VP8, VP9, AV1, Opus, WebVTT, PGS), from the HTTP stream URLs PearTube's worklet serves on 127.0.0.1. Discs, tuners, capture devices and network protocols other than HTTP are out of scope.

## Layout

|Path|Purpose|
|---|---|
|`crates/player`|The engine: source, demux, decoder choice, clock, sync, seek, tracks, and the platform backends (Android, Apple, headless)|
|`crates/codecs`|`register_all`: local replacement decoders register before OxideAV (`first_decoder` uses registration order, not capability priority); replacement container factories register last because they replace entries by name|
|`crates/codec-*`|Decoders and demuxers OxideAV lacks. Same shape as an OxideAV crate: implement `oxideav_core::Decoder` / `Demuxer`, export `register(&mut RuntimeContext)`|
|`crates/e2e`|Corpus runner: plays every corpus file through the headless backend and checks it against FFmpeg; writes `target/e2e/codecs.json`|

## Design

- **Source**: `oxideav-http`'s `HttpSource` (HTTP/1.1 Range, `Read + Seek`) behind a bounded read-ahead ring. Reads have a deadline, so a suspended worklet fails reads instead of hanging them; resume reopens at the last offset.
- **Demux**: `ContainerRegistry::probe_input` (rewinds after reading up to 256 KiB), then `open_demuxer(name, input, &codecs)`. One demux thread fills per-stream packet queues bounded in both duration and bytes.
- **Packet metadata and seeking**: snapshot `Demuxer::packet_metadata()` with each packet, including queued-byte accounting for owned cue metadata. A container random-access point is separate from the codec parser's keyframe flag; the engine and both native decoder gates accept either without changing parser flags. The open-GOP Matroska regression requires all 100 post-seek frames to match FFmpeg, not merely resumed output. Metadata transport is in place; generic audio trimming and WebVTT settings rendering remain incomplete.
- **Buffering**: the read-ahead ring reports when a read waits for bytes that have not arrived. The clock holds at the start until the first audio and video are decoded and about a second is queued (or the input ends), likewise after a seek, and mid-stream whenever a pipeline runs dry while the source is starved, until a second is queued past the clock again. `buffering` in the state follows the hold; `play`/`pause` stay the user's intent, so a pause during buffering stays paused when the data arrives.
- **Audio**: always decoded in software (OxideAV or `codec-*`) to PCM. Android plays it through AAudio, Apple through `AVSampleBufferAudioRenderer`. The sink's presented position is the master clock; Android maps AAudio frame/time pairs onto `CLOCK_MONOTONIC`, clamped to the audio actually queued. Pause and buffering stop the audio output, including while its final queued samples drain. After a seek, only audio from the new seek generation may lead; without audio, or after it ends, the free clock continues from the current position.
- **Video**: the platform decoder first, chosen by trying it: Android `MediaCodec::from_decoder_type` + `configure` on the slot's `ANativeWindow` (the NDK has no codec-list API below API 36); Apple enqueues compressed `CMSampleBuffer`s on `AVSampleBufferDisplayLayer`. Any failure, at open or mid-stream, tears the platform decoder down and continues in software from the next keyframe. Software frames go to Android as RGBA_8888 through `ANativeWindow_lock` (after `oxideav-pixfmt` conversion), and to Apple as `CVPixelBuffer` sample buffers on the same layer.
- **Native packet framing**: AVC/HEVC packet framing comes from the stream's configuration, not a per-packet start-code guess. A valid AVCC length of 256–511 starts with `00 00 01`; misreading it as Annex B corrupts the native decoder's input.
- **Presentation**: software frames wait against the live master clock, with a sink-specific enqueue lead. Android recomputes each MediaCodec release target from the clock rather than committing a distant, uninterruptible deadline. Apple attaches the video renderer to the audio's `AVSampleBufferRenderSynchronizer`; video-only playback uses its own timebase anchored to the engine clock. Neither Apple path uses `DisplayImmediately`. Layer work stays on the main thread, and a failed layer (`requiresFlushToResumeDecoding`) is flushed and re-fed from a keyframe.
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

### Audio-master timing

`cargo test -q -p player` includes two strict realtime flash/beep scenarios:
a 32-second run with the audio device 2% slow, and HTTP starvation followed
by pause and seek. The headless backend records independent frame-arrival
times and PCM playback runs; every expected flash must be within 40 ms of
its actual beep samples. CSV artifacts go in
`$CARGO_TARGET_DIR/engine-sync/` (otherwise `target/engine-sync/`). Run the
suite three times for acceptance. Do not overlap realtime measurements with
builds or throughput benchmarks. Dropped flashes or late host timer wakeups
are failures, not permission to loosen the bound.

On macOS, the dedicated realtime audio/video workers use a scoped Mach
time-constraint policy for paced waits and audio device writes: 20 ms
period, 1 ms computation budget, 2 ms constraint. Codec decoding and
application event callbacks remain under ordinary scheduling. A real-Player
regression holds the first device write until demux EOF, then checks the
audio-priming callback's actual Mach policy. Mach policy changes permanently opt a pthread
out of QoS, so only these owned workers opt out at creation; a guard refuses
to modify a borrowed QoS-managed thread. Each timed scope restores the
previous Mach mode/precedence on exit or unwind and releases its Mach send
right. Non-realtime decoding, subtitle workers and application threads are
not changed. Native tests check the budget and restoration, not just the
configuration constants.

Native harnesses use an eight-second H.264/PCM clip with a per-frame binary
identifier in its top eight pixel rows. Generate it with FFmpeg:

```sh
ffmpeg -v error -nostdin -y \
  -f lavfi -i "color=black:size=160x96:rate=25,drawbox=color=white:t=fill:enable='lt(mod(t,1),0.039)',geq=lum='if(lt(Y,8)*lt(X,128),if(bitand(N,pow(2,floor(X/16))),220,32),lum(X,Y))':cb='cb(X,Y)':cr='cr(X,Y)'" \
  -f lavfi -i 'aevalsrc=if(lt(mod(t\,1)\,0.04)\,0.5*sin(2*PI*1000*t)\,0):s=48000' \
  -t 8 -c:v libx264 -preset ultrafast -g 25 -bf 0 -pix_fmt yuv420p \
  -c:a pcm_s16le -ac 2 -reserve_index_space 4096 \
  -cluster_size_limit 100000 -cluster_time_limit 200 flash-beep.mkv
```

On macOS, `cargo run -p player --example apple_play -- flash-beep.mkv`
runs the real Player and AppleBackend. Add `--software` to force engine
decoding or `--transport` for pause/seek. The harness pauses at sample points
and reads the renderer's displayed pixel buffer, comparing its identifier
with the audio timebase while inside the media duration. Hardware-compressed
readback formats (such as Apple's `&8v0`) are converted by VideoToolbox into
reused linear NV12 storage before CPU inspection; the source is still the
displayed buffer, not a decoder input frame. No screen capture is used.
Nil/unreadable readback is a hard failure; renderer queue counts and zero
accumulated-delay counters alone are not timing proof. These are sampled
displayed-frame offsets, not a continuous presentation-time distribution.

For Android, use the NDK compiler and a 16 KiB-compatible executable link:

```sh
NDK="$HOME/Library/Android/sdk/ndk/27.1.12297006/toolchains/llvm/prebuilt/darwin-x86_64"
CC_aarch64_linux_android="$NDK/bin/aarch64-linux-android29-clang" \
AR_aarch64_linux_android="$NDK/bin/llvm-ar" \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/bin/aarch64-linux-android29-clang" \
cargo rustc -q -p player --target aarch64-linux-android --features probe \
  --example android_sync -- -C link-arg=-Wl,-z,max-page-size=16384
adb -s emulator-5554 push "$CARGO_TARGET_DIR/aarch64-linux-android/debug/examples/android_sync" /data/local/tmp/engine_sync
adb -s emulator-5554 push flash-beep.mkv /data/local/tmp/engine_sync.mkv
adb -s emulator-5554 shell 'chmod 755 /data/local/tmp/engine_sync && PEARTUBE_SYNC_TRACE=1 /data/local/tmp/engine_sync /data/local/tmp/engine_sync.mkv'
```

The debug-only trace records native AAudio frame/time pairs and video
release targets. Software traces also record entry, conversion, window-lock
and post times, separating a late engine wake from rendering or callback
delay. ImageReader callbacks read each displayed frame's identifier and
compare arrival with AAudio frame/time pairs. The audio frame-to-media-PTS
mapping still comes from the engine; these traces do not independently
verify the identity or timing of the audible PCM content. This measures a native surface consumer, not physical display
scanout. The compressed harness uses the emulator's `c2.android` MediaCodec;
`--software` exercises engine decode plus `ANativeWindow`, and `--transport`
exercises pause/seek. Report offsets after the initial second, including
outliers; reaching `Ended` alone does not pass sync acceptance.

On a 16 KiB-page emulator, inspect executable `LOAD` alignment with
`llvm-readelf -l`. An incompatible executable can fail in the loader before
`main`; that is not an engine crash. Successful probe launch does not prove
the app's native libraries are compatible: integration must check every
linked native library and the final app separately.

SubViewer 1 and VPlayer reference checks compare every decoded cue's text, start and end against FFmpeg, including the final open-ended cue. SubViewer 1 keeps its native whole-second timestamps; both formats preserve FFmpeg's negative final-duration sentinel through its unsigned-millisecond display-time conversion.

Codec-version rows require genuine inputs: generated `wmv1_wma1.asf` covers WMV1/WMA1, FATE `vc1/SMM0005.rcv` covers WMV3, and `sipr/sipr_5k0.rm` covers RV10. WMV2, VC-1 and RV20 files are not evidence for those earlier or different codecs.

WMA v1/v2 decoder opening rejects zero sample rates and channel counts before deriving block sizes. The robustness suite covers both invalid dimensions with variable-block coding enabled; valid ASF playback remains covered by the production-registry corpus.

The AVI fork preserves WAVEFORMATEX block alignment for WMA v1/v2 decoder initialization. Production-registry regressions and actual Player playback cover both codecs; the existing AVI restriction requiring zero `dwSampleSize` for WMA remains. Generated regression files explicitly set that field to zero rather than claiming unsupported ordinary FFmpeg AVI headers work.
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
