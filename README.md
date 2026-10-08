# peartube-media

PearTube's media player. It replaces libVLC in the PearTube app (Android, macOS, iOS) with a Rust engine built on the [OxideAV](https://github.com/OxideAV) codec framework, plus the platforms' own video decoders.

## Goal

Play every format on VLC's published feature list (videolan.org/vlc/features.html) plus the modern ones VLC 3 plays (HEVC, VP8, VP9, AV1, Opus, WebVTT, PGS), from the HTTP stream URLs PearTube's worklet serves on 127.0.0.1. Discs, tuners, capture devices and network protocols other than HTTP are out of scope.

## Layout

|Path|Purpose|
|---|---|
|`crates/player`|The engine: source, demux, decoder choice, clock, sync, seek, tracks, and the platform backends (Android, Apple, headless)|
|`crates/codecs`|`register_all`: local replacement decoders register before OxideAV (`first_decoder` uses registration order, not capability priority); replacement container factories register last because they replace entries by name. Decode-only: it turns on `oxideav-core`'s `decode-only` feature, so no OxideAV encoder or muxer is registered|
|`crates/codec-*`|Decoders and demuxers OxideAV lacks. Same shape as an OxideAV crate: implement `oxideav_core::Decoder` / `Demuxer`, export `register(&mut RuntimeContext)`|
|`crates/e2e`|Corpus runner: plays every corpus file through the headless backend and checks it against FFmpeg; writes `target/e2e/codecs.json`|

## Design

- **Source**: `oxideav-http`'s `HttpSource` (HTTP/1.1 Range, `Read + Seek`) behind a bounded read-ahead ring. Reads have a deadline, so a suspended worklet fails reads instead of hanging them; resume reopens at the last offset. Plain http only by default: the app reads every stream from its worklet on 127.0.0.1, so the `oxideav-http` fork builds without its `tls` feature and an `https://` URL fails to open with an error naming it. The player's `https` feature turns TLS back on.
- **Demux**: `ContainerRegistry::probe_input` (rewinds after reading up to 256 KiB), then `open_demuxer(name, input, &codecs)`. One demux thread fills per-stream packet queues bounded in both duration and bytes.
- **Packet metadata and seeking**: snapshot `Demuxer::packet_metadata()` with each packet, including queued-byte accounting for owned cue metadata. A container random-access point is separate from the codec parser's keyframe flag; the engine and both native decoder gates accept either without changing parser flags. The open-GOP Matroska regression requires all 100 post-seek frames to match FFmpeg, not merely resumed output. Metadata transport is in place and audio trims are applied (below); WebVTT settings rendering remains incomplete.
- **Buffering**: the read-ahead ring reports when a read waits for bytes that have not arrived. The clock holds at the start until the first audio and video are decoded and about a second is queued (or the input ends), likewise after a seek, and mid-stream whenever a pipeline runs dry while the source is starved, until a second is queued past the clock again. `buffering` in the state follows the hold; `play`/`pause` stay the user's intent, so a pause during buffering stays paused when the data arrives.
- **Audio**: always decoded in software (OxideAV or `codec-*`) to PCM. Android plays it through AAudio, Apple through `AVSampleBufferAudioRenderer`. The sink's presented position is the master clock; Android maps AAudio frame/time pairs onto `CLOCK_MONOTONIC`, clamped to the audio actually queued. Pause and buffering stop the audio output, including while its final queued samples drain. After a seek, only audio from the new seek generation may lead; without audio, or after it ends, the free clock continues from the current position.
- **Audio trims**: the encoder delay and end padding a container declares (`PacketMetadata::audio_trim`: MP4 edit lists and iTunSMPB, MP3 LAME/Xing info and iTunSMPB, Ogg Opus pre-skip and end granules, Matroska CodecDelay and DiscardPadding) come off the decoded PCM once, after decoding, as FFmpeg's `decode.c` removes them, and what follows a start skip plays from the skip's end on the packet's timeline. The Opus pre-skip is the decoder's start delay, which a container's skip replaces. OxideAV's Vorbis decoder never outputs its first packet, the frame FFmpeg's decoder outputs and drops as its delay, so a skip on that packet (the CodecDelay FFmpeg's encoders write) is already applied. `crates/audio-trim` does this for both the engine and `refcheck`, so reference tests check what plays. Its input limits (64 packets awaiting output, 32 MiB of held padding) never stop the audio: those samples play untrimmed and `State::audio_trim_fallbacks` counts them. MP4 trims stay in the media timescale, so a skip ends where the edit list says even when the timescale is not the output rate; FFmpeg 2da55bf applies the ticks as output samples there.
- **Video**: the platform decoder first, chosen by trying it: Android `MediaCodec::from_decoder_type` + `configure` on the slot's `ANativeWindow` (the NDK has no codec-list API below API 36); Apple enqueues compressed `CMSampleBuffer`s on `AVSampleBufferDisplayLayer`. Any failure, at open or mid-stream, tears the platform decoder down and continues in software from the next keyframe. Software frames go to Android as RGBA_8888 through `ANativeWindow_lock` (after `oxideav-pixfmt` conversion), and to Apple as `CVPixelBuffer` sample buffers on the same layer.
- **Native packet framing**: AVC/HEVC packet framing comes from the stream's configuration, not a per-packet start-code guess. A valid AVCC length of 256–511 starts with `00 00 01`; misreading it as Annex B corrupts the native decoder's input.
- **Presentation**: software frames wait against the live master clock, with a sink-specific enqueue lead. Android recomputes each MediaCodec release target from the clock rather than committing a distant, uninterruptible deadline. Apple attaches the video renderer to the audio's `AVSampleBufferRenderSynchronizer`; video-only playback uses its own timebase anchored to the engine clock. Neither Apple path uses `DisplayImmediately`. Layer work stays on the main thread, and a failed layer (`requiresFlushToResumeDecoding`) is flushed and re-fed from a keyframe.
- **Lifecycle**: `open` takes no surface; `set_surface` attaches or detaches one (Android `SurfaceView` callbacks, macOS/iOS view moves). `suspend` / `resume` follow the activity: stop audio, release the platform decoder and surface, resume at the last position.
- **Subtitles**: text, ASS and bitmap subtitles render to RGBA on an overlay above the video (a second `SurfaceView` on Android, a `CALayer` on Apple). Bitmap frames are display states: `VideoFrame::display_duration` supplies a known end; otherwise the next frame replaces the state, with a blank frame clearing it. Packet duration is not a bitmap display timeout. Bitmap coordinates use the subtitle canvas size, independently of the video's resolution.
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

The player never encodes or muxes. `crates/codecs` turns on the core fork's `decode-only` feature: `CodecInfo::encoder` and `ContainerRegistry::register_muxer` drop their factories, so nothing references OxideAV's encoders and muxers and LTO removes them (Android `libmain.so`: 27.3 MB to 21.0 MB stripped, 13.2 MB to 9.9 MB deflated in the APK).

Release builds compile dependencies and this workspace's cold crates at `opt-level = "s"` and the code that must keep its speed at 3: the H.264, HEVC, AV1, VP8, VP9, MPEG-1/2, MPEG-4 Part 2, VC-1/WMV (`codec-wmv`), AAC, Opus, MP3, AC-3 and DTS (`codec-dca`) decoders, `oxideav-core` (bit readers), `oxideav-pixfmt` (RGBA conversion of software frames), the Ogg demuxer (it runs per packet; at `"s"` Opus in Ogg took about 0.4% more instructions) and `player`. Binaries stay at 3: fat LTO optimises the whole program at the final crate's level, and at `"s"` rustc turns loop and SLP vectorization off for all of it. The app's `mobile/Cargo.toml` carries the same lists (there every dependency not named is `"s"`; here a workspace crate not named is 3), so a new crate goes on one of them in both files. This saves another 1.5 MB raw, 1.1 MB deflated.

Leaving TLS out (rustls, ring and the webpki roots, through `oxideav-http`'s `tls` feature) saves another 0.86 MB raw, 0.54 MB deflated, in the Android `libmain.so`.

### Matroska packet compatibility

The MKV fork follows at most two SeekHeads (one target per other master),
recovers complete packets from damaged Cluster tails, reconstructs ProRes/WavPack
payloads, and derives lace timing, TrackTimestampScale and H.264/HEVC decode
timestamps. Untrusted input is bounded: 256 tracks, 4 MiB per CodecPrivate and
16 MiB in total, and 32 MiB retained per Block (payload capacity, laces,
header stripping, copies, side data and packet slots). BlockGroup children
must fit their parents, and their stored bytes (read into buffers of exact
size) and records are charged to that budget before they are read or held; a
Block waiting for queue room stays within it. Duplicate BlockAddIDs are
dropped in linear time. A Top-Level master is read for a CRC-32 only when its
first child is one, then in fixed chunks; a third SeekHead, or a SeekHead,
Tracks or Tags master larger than its budget (about 184 KiB, 32 MiB, 32 MiB),
is refused before any of it is read. A CRC-32 on an Info, Cues, Chapters or
Attachments master is still checked over its whole body (small heap, but a
known network cost: their declared size has no limit). Optional metadata never
stops playback: damage in Chapters, Attachments, Tags, Cues, a SeekHead or any
other Top-Level master but Info and Tracks drops it, or cuts it to the records
before the damage, with one damage event, in strict opens too; damage in the
EBML header, the Segment, Info or Tracks still fails a strict open. Junk where a
Top-Level element should start, before the first Cluster, is skipped by either
open with a forward scan, as FFmpeg's matroska_resync does, one damage event per
run; the scans read at most 1 MiB in total, and a strict open fails only when
one ends without finding an element. Every element in a Tracks, Tags, Chapters,
Cues or SeekHead tree, and in the EBML header, must fit its parent, and the EBML
header's strings and extension records keep at most 16 MiB together (FFmpeg's
limit for one EBML string). Only a Segment or Cluster may use the unknown size;
any other Top-Level element doing so, or running past its Segment, is damage
(the walk rescans for what follows; between Clusters, playback and seeks resume
at the next Cluster), and so is an AttachedFile or FileData of unknown size,
which leaves the UIDs of the attachments kept before it. A master over its
budget, in line or found through the SeekHead, is noted as damage too, and a
Tags or SeekHead found through the SeekHead keeps its complete records. A
Cluster may start with a Void.
SegmentUUID, PrevUUID and NextUUID must be 16 octets. Text fields in Info,
Chapters and Attachments hold at most 64 KiB, and each of these masters keeps
at most 1 MiB. Past it, Chapters and Attachments keep the records that fit, in
order: a long chapter list never stops playback. Attachment payloads are never
read at open; one fetched on request grows only as bytes arrive, a payload
reaching past its AttachedFile or the Segment is refused unread, and a source
failure while it reads is returned as itself. Everything the open keeps from
Tracks or Tags, including tag resolution, the per-stream views and room to
parse or decompress a codec configuration, stays within the master's 32 MiB
limit at its peak and after the open; a CodecPrivate decodes in the room kept
for parsing it and its stored form's charge passes to the decoded one, and a
Tags master replaced between Clusters frees its entries' room in one pass. The
Cues index keeps at most 32 MiB:
past that, or past damage, it keeps the CuePoints before it, and a seek past
the last point kept for its track, or with no Cues, scans the Clusters from
the first one (a known read cost on large remote files). Cluster
records, their index, the EncryptedBlocks and SilentTracks numbers they keep,
and the CRC-32 statuses share one 32 MiB budget, lists included, and are
recorded once even when a seek revisits them. A block past the budget is
damage and the walk resumes at the next Cluster; a Cluster past it gets no
record or status while playback and seeks go on. At most 4096 damage events
are kept, and the rest are counted exactly.
The 1024-packet cap counts
virtual-track copies and the frames a lace actually holds (a one-frame EBML
lace is InvalidData): compliant Blocks wait until held packets drain, and one
that then fails recovers from its own offset; an individual Block needing more
than 1024 packets is InvalidData and queues nothing. Startup analysis also
stops at a 512 KiB retained-byte threshold, plus the bounded current Block.
Source errors propagate, including ordinary InvalidInput reads,
source-generated UnexpectedEof and failures met in trailing Tags, Cluster
CRC-32s or seek landings; physical truncation remains recoverable.

Packets keep FFmpeg's parser keyframe flags. The shared
`Demuxer::packet_metadata()` snapshot separately carries the container's
random-access signal on lace 0. The producer test checks all four Cues
points, including parser-keyframe=false/container-keyframe=true at 4 s.
The actual Player regression compares every one of FFmpeg's 100 post-seek
frame MD5s. **The engine/native-gate consumer integration is separate and
not included on this branch**; the unchanged engine still fails this test.

Duration-less AAC/HE-AAC, MP3 (including MPEG-2), AC-3/E-AC-3 and DTS core
laces now match strict FFprobe packet comparisons on nine generated/real
fixtures, including millisecond time bases. AAC 960-sample/LD/ELD/USAC and
14-bit/substream-only DTS frame timing are not inferred.

Matroska/WebM WebVTT packets carry raw cue text to the `subs-text` WebVTT
decoder (the FFmpeg port standalone `.vtt` files use), timed by the packet.
`Demuxer::packet_metadata().webvtt` replaces the typed-only accessor and
preserves each cue's identifier/settings through lacing and seeks. The
accessor clears before the next read/seek, including errors and EOF;
previously captured owned snapshots remain valid. Standalone `.vtt` files
give each cue the same metadata (FFmpeg's side data) and their `WEBVTT`,
`STYLE` and `REGION` blocks as extradata. MP4 `wvtt` samples (ISO/IEC
14496-30, which FFmpeg does not read) decode from their `vttc` boxes. The
player places WebVTT cues as W3C WebVTT section 7 does: the line (snapped to
lines or a percentage, with its alignment), position, size and alignment
(`start` and `end` by the text's direction), vertical cues, regions from
the header (on the region's box, clipped to its `lines`, so `lines:0`
shows nothing; `scroll:up` moves the lines up over 0.433 s with CSS's
`ease`), and the moves that keep cues off each other; a cue with no room
is not shown. It draws the cue's own text as browsers do: the
`rgba(0,0,0,0.8)` cue box, the `::cue` rules of `STYLE` blocks (colour,
background, weight, style, decoration, shadow, opacity, relative size;
class, voice, language, identifier and type selectors, descendants),
right-to-left paragraphs reordered by the Unicode Bidirectional Algorithm,
and ruby text above its base. The bitmap font has one face, so
`font-family` changes nothing. `cargo test -p player --test
webvtt_placement --test webvtt_style` checks placement and drawing through
`.vtt`, FFmpeg's Matroska and WebM remuxes and hand-built MP4s; `cargo
test -p subs-text --test webvtt_settings` mutates settings, headers, MP4
samples, cue text and style sheets at least 2000 times each.

`cargo test -p check-mkv -p player --no-fail-fast` compares packet fields
directly with FFmpeg 9, checks incremental reads and malformed input, and
exercises player subtitle dispatch. Known-wrong packet digests are not accepted;
every packet field of the 69 samples equals FFmpeg's, CodecDelay-shifted
timestamps included. The MKV demuxer exposes CodecDelay, DiscardPadding and
SeekPreRoll as `PacketMetadata::audio_trim` for the AudioTrim consumer, which
lives on another branch.

### Dirac and legacy video decoding

`demux-misc` opens raw Dirac and VC-2 (`.drc`) by content probe. Its LGPL
port of FFmpeg 2da55bf's Dirac parser preserves packet bytes and key flags.
Timing differs on purpose: FFmpeg treats the parser's picture numbers as
ticks of 1/1200000 s; the port scales them by the sequence frame duration,
so a 30-picture, 30 fps stream plays for one second. The stream time base
stays 1/1200000. The first sequence header supplies the frame rate, with
25 fps as the fallback.

The Dirac fork honors reference retirement, keeps up to eight reference
pictures and returns pictures in display order through FFmpeg's delay
buffer. Flushing emits the pictures still waiting; a seek reset drops
them and clears the reference and output state. A new sequence drains the
old sequence and restarts its picture count, unlike FFmpeg.

Known seek limit (deferred): a raw Dirac seek can reset away the sequence
header and land on a picture-only packet. Pictures may then be dropped
until another sequence header arrives. The sequential playback checks
below do not cover this recovery path.

`cargo test -p check-decoders --test video` compares every decoded frame
with pinned FFmpeg: Dirac main/low-delay, MJPEG including interlaced and
4:2:2 files, 24-bit Cinepak, H.261 and Indeo 3. Indeo 3 emits native
`yuv410p`, supported by the core and pixel-conversion forks. Palettized
Cinepak remains outside this coverage; its palette mode cannot yet be
selected through the stream parameters.

`cargo test -p demux-misc --test reference dirac` checks packet bytes, key
flags and frame-unit timing. `cargo run -j 2 -p e2e --release -- --filter
dirac` plays both raw profiles through the headless Player from disk and
HTTP; each has 30 frame MD5s equal to pinned FFmpeg.

## Licenses

Code in this repository is MIT unless a crate says otherwise. Decoders with no public specification (TrueHD/MLP, several Windows Media and RealMedia codecs, DVD and Blu-ray LPCM in `codec-lpcm`) are ports of FFmpeg's LGPL-2.1-or-later decoders; each such crate is LGPL-2.1-or-later, carries its own LICENSE, and ports only FFmpeg files whose headers say LGPL. The audio-trim producers in the MP4, MP3 and Ogg forks and the FFmpeg-exact ADPCM and G.726 decoders in the ADPCM fork are such ports too; those crates are `MIT AND LGPL-2.1-or-later`, with the ported files marked.

## Verification

`cargo run -p e2e --release` plays the corpus (FFmpeg's FATE samples, generated files, and files from FFmpeg's sample archive that `corpus/fetch-samples.sh` fetches and `corpus/samples.sha256` pins) through the headless backend and compares every stream with FFmpeg: `framemd5` for bit-exact codecs, PSNR/SNR thresholds for the rest. Every format on the list needs a passing file. The result is `target/e2e/codecs.json`.

`cargo test -p player --lib engine::subtitle_tests` checks PGS, DVB and DVD/VobSub (paired, MPEG-PS and Matroska) show/replacement/clear media times against FFmpeg with an injected clock, pinning each boundary to FFmpeg's microsecond rounding (±0.5 µs; the engine keeps exact 90 kHz times), and compares every complete subtitle canvas with sub2video, including final DVB and DVD expirations at EOF. `cargo test -p player --test subtitle_timing` independently checks the real Player's PGS state sequence and canvases. These are logical-timing and integration checks, not a demonstrated wall-clock presentation-latency bound; under shared-machine load, a requested 5.9 ms wait took 65 ms and a 100 ms wait took 313 ms.

`cargo test -p subs-bitmap` compares PGS (SUP, Matroska and M2TS), DVB (MPEG-TS and Matroska), and DVD/VobSub (paired files, MPEG-PS, ordinary and zlib-compressed Matroska) with FFmpeg, including every complete RGBA canvas and its display interval. DVB uses all 46 display states in FATE `sub/dvbsubtest_filter.ts`, the only FATE DVB sample (`tests/fate/subtitles.mak`); generated transport streams add two services on one PID and malformed segments (a display definition with a cut-off window, a region 20000 pixels wide, a cut-off map table under a computed CLUT), each compared with FFmpeg. 8-bit pixel strings, map tables, display-definition window offsets and the non-modifying colour have only unit expectations. Transport and paired VobSub tests also compare every packet's timestamps and payload MD5; index variants check FFmpeg's `size:` (sscanf) and palette (strtoul) reading. Robustness tests exercise 7200 real PGS packet mutations, 4800 real DVB packet mutations, 4800 real DVD packet mutations with full FFmpeg recovery comparisons, and 2000 VobSub index mutations. The MPEG-TS fork recognizes private-PES DVB descriptor 0x59 and retains its language, composition/ancillary page IDs and subtitle type. The existing OxideAV VobSub tests pin upstream gaps, independently of the replacement DVD decoder's differential tests.

Bitmap subtitle input is bounded where FFmpeg is not: canvases (a PGS presentation, a DVB display definition, a VobSub `size:`) of at most 4096×4096, DVB regions of at most 4096×4096 pixels in all, 1024 DVB object placements and a per-packet bound on DVB painting (every pixel a region allocation or fill writes, and every object placement), one blank DVB render per packet, and CVD/OGT regions no larger than their canvas, decoded only as far as the canvas shows them. `cargo test -p subs-bitmap --test budgets` feeds the hostile cases (an 8192×8192 display definition followed by a thousand 7-byte end-of-display packets, an OGT header declaring 16383×4096, 65,280 placements painted from 64 KiB of object data, 1,024 fills or resizes of a 1024×1024 region in one 16 KiB packet) under memory and time budgets. A DVB stream decodes only its first service's composition and ancillary pages, as VLC does, and skips page compositions on an ancillary page that is not also the composition page, as VLC's dvbsub.c does; FFmpeg's default decodes every page.

A blank bitmap state cancels any previous timeout and has no pending expiration of its own; even when DVB labels it with a page timeout, it must not delay EOF or emit a redundant clear.

`subs_bitmap::open_vobsub(idx, sub)` accepts two explicit `Box<dyn ReadSeek>` inputs. It never guesses a sibling filename or reads a path from an untrusted index. It retains the index palette, language, timestamps and split-SPU boundaries; seeking returns the preceding indexed subtitle. `crates/codecs` installs `subs_bitmap::register_codecs` before oxideav-sub-image, whose decoders claim the same ids, and `register_containers` after it. Matroska `S_VOBSUB`, MPEG-PS DVD subpicture units and paired VobSub use decoder ID `dvd_subtitle`; `dvdsub` (FFmpeg's decoder name) and `vobsub` are also claimed.

`Player::open` on a `.idx` URL plays the index with the program stream beside it: the `.sub` of the same name, named as FFmpeg's vobsub demuxer names it (`IDX` becomes `SUB`, any other case `sub`), for files and HTTP alike (`cargo test -p player --test subtitle_canvas paired_vobsub`). The corpus runner opens the pair the same way.

`cargo test -p subs-bitmap --test vcd --test vcd_spumux -- --nocapture` compiles the original VLC C CVD/OGT decoders, bit reader and YUVP-to-RGBA converter at revision `2e358f3098c2f2b7621d1dc568de8b61ad786322` and compares every complete RGBA canvas and display interval with the Rust ports. Set `VLC_SRC` to that checkout (default `~/projects/vlc-src`); `cc` is required. The adapter supplies callbacks/types and places converted regions on the canvas, clipped at its edges; it does not replace parsing, RLE or palette conversion. That placement is harness code mirroring the port's, so the comparison covers decoding, colours and timing, not VLC's on-screen geometry: VLC's renderer also scales each region by its sample aspect ratio (`vout_subpictures.c`), which OGT sets from the region's size (`svcdsub.c`). Test output prints exact compiler and replay commands, and retains encoded `.packets` inputs and complete timing/rectangle/RGBA `.rgba` outputs under `CARGO_TARGET_TMPDIR/subs-bitmap-vlc-<pid>`. `vcd` uses hand-authored structural packets (fragmentation, truncated image data, colours without a palette entry, unchecked OGT packet numbers, regions leaving the canvas) plus 4,800 mutations. `vcd_spumux` reads CVD and SVCD files authored by an independent encoder, dvdauthor 0.7.2 `spumux` (`tests/data/spumux/generate.sh` records the exact invocation), through the production MPEG-PS demuxer, including three-packet subtitles. Neither is an archived disc stream: none has been found, so interoperability with real discs remains unproven, and no FFmpeg CVD/OGT parity is claimed (FFmpeg has neither decoder). VLC, and therefore this port, renders spumux CVD colours with Cb and Cr exchanged (spumux writes Y, Cr, Cb; VLC reads Y, Cb, Cr) and shows each spumux CVD subtitle for 5.86 s, reading the `04 08 0c 10` spumux appends after the recorded unit size as a duration field.

DVD, CVD and OGT subtitles place regions in video pixels; FFmpeg's canvas for a stream that declares no size is the video's (`fftools/ffmpeg_demux.c`), and VLC places CVD/OGT regions on the video unscaled. `spawn_subtitles` decides the canvas once, as the subtitle pipeline starts, and never waits for one: a stream declaring no canvas gets the selected video's size from `State::video_size`, and the DVD decoder's own `size:` (read as FFmpeg reads it) takes precedence over that. DVB/PGS define their canvas in-band. `State::video_size` holds the container's dimensions at open; nothing publishes decoded ones, so where those are unknown (an MPEG-2 sequence header past the MPEG-PS scan, H.264 in a VOB, a Matroska `PixelWidth` 0 without an avcC/hvcC record to read the size from) the decoders keep their 720×576, as in subtitle-only playback, with every region at FFmpeg's pixel position. FFmpeg uses the decoded video size there. Closing that gap needs the video pipeline to publish `Decoder::output_video_dimensions` (oxideav-core 96094a9; the pinned mpeg12video fork implements it) before the subtitle decoder opens, and the subtitle pipeline to wait for it without holding its lane; `unknown_at_open_ps_video_publishes_canvas_before_first_dvd_cue` is ignored with that reason.

Subtitles never hold back video or audio. Behind them the subtitle lane drains as the demuxer fills it, whatever its cues' starts: a cue waiting for the clock would otherwise stop the demuxer short of the audio the clock needs to reach it. Decoded cues wait in a queue of at most 64 cues (64 MiB), the latest due going first, and come up by start time, as VLC selects subpictures by date. Subtitle-only playback keeps the clock's pace instead, one decoded cue waiting at a time. Text cues render in the video's size, scaled down to at most 4096×4096 pixels' worth (8K video renders text in 5461×3072), and are cropped to their visible pixels; at most 64 (64 MiB) are up at once, the earliest up going first. So no single cue, text or bitmap, can outgrow 64 MiB. A playback with video or audio ends with the last subtitle state down, realtime or not, however far ahead its own end is, even when its subtitle pipeline first ran after them; subtitle-only playback plays to its last end.

Selecting a subtitle track never seeks: the new track shows from the next cue the demuxer reads after the switch. The demuxer has already read up to about two seconds of media ahead of playback (the queue bound) and dropped the new track's packets in it, so a cue up at the switch, and any cue starting in that read-ahead, is not shown; the cue after them is. Only an audio switch re-reads from the clock's position, as before. The audio track a playback picks by default stays selected through any selection of another kind, including one made while the Player opens. A playback whose audio is switched on after its subtitles started ends with the subtitles cleared, as any playback with video or audio does.

`cargo test -p player --test subtitle_canvas -- --nocapture --test-threads=1` exercises actual video and subtitles through Player. Known-dimension cases compare all 180 CVD video frames and three 352×480 canvases, all 180 SVCD video frames and three 480×480 canvases, and ten NTSC DVD video frames plus the first 720×480 cue. A VobSub case retains its explicit 720×480 subtitle canvas over 352×240 video; one whose `size:` has no height takes the video's canvas, as FFmpeg's sscanf does. Unknown video sizes (a VOB whose MPEG-2 sequence header lies past the scan, MPEG-2 in Matroska declaring 0×0) with cues 2.6 s apart play to the end, realtime or not, with every video frame equal to FFmpeg's and both cues on the 720×576 fallback. In FFmpeg's H.264 VOB neither FFmpeg nor the PS demuxer reads a subtitle packet; selecting that stream holds nothing up and changes none of the frames decoded. Those are 19 of FFmpeg's 20 frames, with or without a subtitle: `h264_vob_plays_every_ffmpeg_frame` keeps that comparison and is ignored until the H.264-in-PS video path is fixed. The headless sink packs frames at the container's size, so these cases hash each presented frame at FFmpeg's 720×480 picture size. A DVD cue moved ten hours ahead leaves the rest of the playback intact. Video hashes match FFmpeg's simple-IDCT reference; complete subtitle canvases match native VLC or FFmpeg. Generated inputs, captured RGBA canvases and hash reports remain under `CARGO_TARGET_TMPDIR/player-subtitle-canvas-<pid>`. These are encoder/remux fixtures and logical integration evidence, not archived-disc coverage or physical presentation timing. `cargo test -p player --test subtitle_lifecycle` plays a DVB state with a 15 s timeout over 1 s of video, which ends with the video, cleared; an open-ended PGS state, cleared at Ended in realtime and without it, also when its subtitle decoder opens only after the video pipeline has ended; selecting a PGS track mid-cue, which shows the track from its next display set; and seeking into a cue, which shows the cue at once.

`cargo test -p player --test subtitle_av` plays WebVTT text beside PCM audio through a test container that replays a Matroska file's packets in a staged order, seeks, refuses or fails seeks, or holds the demuxer inside its open, before its first packet, or both in turn, until the test lets it go. Subtitle packets demuxed ahead of the audio (audio through 0.5 s, then cues at 1, 3 and 5 s) leave every PCM sample and video frame played and each cue shown from its start. A subtitle switch, on a demuxer that seeks, cannot, or fails, flushes nothing: the PCM equals FFmpeg's, the single-keyframe H.264 decodes unbroken, and only the new track's next cue shows. Selecting a subtitle track keeps the default audio track. Subtitle selections made while the Player opens keep it too, and an explicit audio choice made then stands: with the demuxer held inside its open or before its first packet, and with subtitles selected over and over from before the Player reads its selection until it publishes its tracks (40 opens), so some land while it picks the default track. A subtitle-only playback (audio switched off while the demuxer is held inside its open) whose audio is switched on before its first packet ends with its open-ended PGS state cleared, realtime or not. Eighty overlapping cues beside 1080p video stay at 64 images up at once; the video and audio play on, the flood comes down at its end, two later overlapping cues show together, and a seek clears the flood. The text decoders take Matroska's WebVTT blocks; its SubRip and ASS blocks they reject ("SRT: cue has no valid timing", "ASS: cue missing Dialogue prefix"), so the tests use WebVTT.

Closed captions (EIA-608 and CEA-708 carried as ATSC A/53 caption data in H.264, HEVC and MPEG-1/2 video) are the `subs-cc` crate. The demux loop reads each packet of the playing video as the demuxer reads it, up to two seconds ahead of the clock, and puts its caption data in presentation order (`engine/captions.rs`). When the video codec can carry captions, the playback has two caption streams, `0x10000` (EIA-608, FFmpeg's `cc_dec`) and `0x10001` (CEA-708 service 1, VLC's decoder). Each is listed in `State::tracks` once its data shows up, and is selectable with `select_subtitle`. A caption screen is a display state: it comes up when it changes and stays until the next screen replaces it, as VLC shows captions, with EIA-608 in FFmpeg's `real_time` mode. QuickTime `c608` tracks are data streams in the MP4 demuxer and are not listed.

`cargo test -p subs-cc` checks the caption data against FFmpeg's `-a53cc 1` side data, read back through its lavfi `movie=…[out0+subcc]` source, picture by picture and at the same times. Inputs: FATE `sub/Closedcaption_rollup.m2v` (A/53 Part 4) and `sub/scte20.ts` (SCTE-20), and the roll-up captions re-encoded into H.264 and HEVC SEI in Matroska and TS. EIA-608 cues equal `ffmpeg -c:s srt` (times, text) and `-c:s ass` (positions, styles) cue for cue, and FFmpeg's `-real_time 1` events, on the video inputs and FATE `sub/witch.scc`. CEA-708 outputs equal those of VLC's unmodified `modules/codec/cea708.c`, built into a test harness from the VLC source tree and driven as `cc.c` drives it, on the video inputs and on three seeded random service streams. 2000+ truncated and bit-flipped real packets per stage never panic. `cargo test -p player --test captions` plays the H.264 captions in Matroska and TS in realtime: both caption tracks are listed, and every image put up is the caption screen current on the clock (FFmpeg's real-time events for EIA-608, the decoder's outputs for CEA-708), at most 300 ms after its time; every screen that stays 300 ms comes up, and the screen is clear at Ended. The bound is loose because the test runs unoptimized, often beside builds; the times themselves are checked exactly in `subs-cc`.

Scenarist Closed Captions (`.scc`, EIA-608 pairs by SMPTE time code) open with the SCC demuxer `subs-text` ports from FFmpeg's `sccdec.c`; every packet of FATE `sub/witch.scc` equals ffprobe's (`cargo test -p subs-text --test scc`). The corpus runner lists the caption tracks of the played video as the engine does and compares the EIA-608 track with FFmpeg's SubRip of the lavfi `subcc` output, in real time mode with the stream's own times (`-copyts -real_time 1`); FFmpeg has no CEA-708 decoder, so that track only decodes there.

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

Standalone subtitle acceptance (`cargo test -p subs-text --test reference standalone_`) compares every cue's visible text, start, end and count through the production registry with FFmpeg's `text` encode of its decode. It does not normalize away raw tags the player would display, and does not claim style fidelity from text equality. All 13 fixture cases pass. They cover 7 formats: SubRip ×5, MicroDVD ×2, SubViewer, MPL2, WebVTT ×2, ASS and SSA. Container probing is a separate diagnostic: `cargo run -p demux-misc --example check_oxideav`; opening a container is not proof of correct packets or playback.

SubRip, ASS/SSA, WebVTT, MicroDVD and SubViewer demuxing and decoding are LGPL ports of FFmpeg's in `subs-text`, registered ahead of OxideAV's (containers by name, so `.srt`, `.ass`, `.vtt`, `microdvd` and `subviewer2` files open with them). Each ported file, including the USF decoder ported from VLC, starts with its upstream copyright lines, the LGPL notice and the dates it was changed. Each decoder converts a cue to an ASS event as FFmpeg does, and one conversion decides what it shows and how: the text FFmpeg's `text` encoder keeps (override blocks hidden; `\h`, `\{` and brace text without a backslash stay literal, as in FFmpeg) styled by the event's style from the script header or CodecPrivate and its `\b \i \u \s`, primary colour, font, first alignment and `\r` overrides. `\r` follows libass, the renderer players and FFmpeg's `ass` filter use: a bare `\r`, or `\r` naming a style the script lacks, returns to the event's own style. FFmpeg's SubRip encoder resets differently (a bare `\r` returns to `Default`), so it is not the oracle for `\r`. Instead, `ass_style_resets_render_as_libass_renders_them` in `player_text` compares the text colours drawn for each cue with the frame FFmpeg's `ass` filter renders. The compositor draws colour, bold, italic and horizontal alignment; vertical alignment, outline, font face/size, `\pos`/`\move` and karaoke timing are not drawn. MPL2 still decodes through the pinned subtitle fork, including omitted end timestamps and italic/bold/underline line prefixes.

The character set is decided per cue. A cue that is valid UTF-8 shows as UTF-8, as in FFmpeg. A cue that is not is read as Windows-1250, where FFmpeg without `-sub_charenc` rejects it. One such cue does not change how the others read. `cargo test -p subs-text --test charset` covers SubRip, ASS, WebVTT, MicroDVD, SubViewer, SAMI, VPlayer and SubViewer 1 files, and Matroska SubRip, ASS and WebVTT packets. MPL2, decoded by the fork, shows invalid bytes as U+FFFD. Font names are cut to 127 bytes, the most FFmpeg's `\fn` override reads, and all runs of a cue share one copy. A cue's memory grows with its length, not with font-name length times run count: `cargo test -p subs-text --test memory` decodes a huge style font across many runs and line breaks, repeated `\r` resets, and a mov_text font table repeating one id, each under an 8 MiB peak.

Text subtitles inside containers play through the Player (`cargo test -p subs-text --test player_text`). FFmpeg- and mkvmerge-generated Matroska SubRip (copied, and converted from ASS), ASS (copied, converted from SubRip, from SSA), mkvmerge `S_TEXT/SSA`, MP4 `mov_text` (from SubRip, and from styled ASS) and WebM/Matroska WebVTT files decode to FFmpeg's cue text and times (Matroska and WebM to the microsecond of `ffprobe -show_packets`). Every cue reaches the subtitle sink, and its render there equals the render of FFmpeg's SubRip conversion of the same cue, CodecPrivate and sample-entry style colours included. That conversion is read by oxideav-subtitle's SubRip parser, not by the decoders under test. Both renders come from the same compositor, so this oracle checks the text, times and styles the decoders hand over; it does not check the compositor's pixels against an independent renderer. The mov_text decoder builds FFmpeg's ASS events too (its sample-entry default style is the `Default` style). QuickTime `.mov` text tracks still fail: the pinned oxideav-mov demuxer, which wins the probe tie with oxideav-mp4 by registration order, exposes them as data streams, so neither the registry nor the Player selects them; oxideav-mp4 exposes the same track as `mov_text`.

Subtitle robustness (`cargo test -p subs-text --test robustness`) mutates real reference packets — demuxed from FATE samples, or from mkvmerge remuxes of reference sources for `S_TEXT/SSA` and USF — 2000 times per codec through the production decoders with the stream's real CodecParameters and preceding packets (ASS script header, mov_text sample entry, Kate headers), then mutates the extradata itself; and sends 1000 mutated or truncated copies of each reference file through its registered demuxer and the decoders, and through every registered probe. One mutation replaces a number in the data with a 32- or 64-bit integer edge. Every fourth packet trial also sets the packet's pts and duration to edge values (absent, `i64::MIN`, `i64::MIN + 1`, -1, 0, `i64::MAX - 1`, `i64::MAX`) and its time base to one of eight, including zero, negative and `i64`-extreme terms. CMML has no reference sample, so its trials mutate a hand-written document. The probe pass currently fails: demux-misc's AC-3 probe (`parse_ac3_header_swapped`) indexes past an odd-length buffer end.

Legacy subtitle demuxing bounds each SubViewer1/VPlayer timestamp component to the format's signed 32-bit field before 64-bit arithmetic. SubViewer1, VPlayer and SAMI order cues by timestamp and original file order, fill missing durations, then remove adjacent exact duplicates, as FFmpeg does. Equal-text cues with different durations remain distinct.

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
