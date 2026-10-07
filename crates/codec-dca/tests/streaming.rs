//! Streaming behaviour of the `dca` decoder and the raw `dts` / `dtshd`
//! demuxers, with FFmpeg as the reference wherever it is one:
//! - FFmpeg's demuxers hand PES payloads to its dca parser, which
//!   reassembles frames across them. The same DTS frames cut into
//!   MPEG-TS PES packets anywhere (inside a sync word, between the core
//!   and its extension substream, inside the substream, several frames
//!   per PES) decode through the player's registry to FFmpeg's PCM, frame
//!   for frame, each at its presentation time: the PTS of the PES its
//!   first byte arrived in when no frame started there before it, else
//!   after the frame before it. That is FFmpeg's timeline too, but for a
//!   PES boundary inside a sync word.
//! - A frame that fails to decode keeps its place on the timeline, as
//!   FFmpeg's parser timing keeps it; a frame whose header gives no
//!   duration leaves the frames after it untimed rather than guessed.
//! - Decoded DTS-HD frames carry FFmpeg's timestamps after its
//!   skip-samples trimming: every dca.mak DTS-HD input, and copies whose
//!   initial padding ends inside a frame or several frames in.
//! - Reassembly holds at most the largest frame the decoder takes, in the
//!   decoder and in both demuxers: a longer frame is an error, the last
//!   one of the input too, and reading goes on after it.

use oxideav_core::{
    AudioFormat, CodecId, CodecParameters, Decoder, Error, Frame, MediaType, Packet, ReadSeek, RuntimeContext, SampleFormat,
    TimeBase,
};
use refcheck::fate;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use codec_dca::decoder::MAX_PACKET_SIZE;

// ───────────────────────── helpers ─────────────────────────

/// Every packet the crate's `format` demuxer cuts from `input`.
fn demux_all(format: &str, input: Box<dyn ReadSeek>) -> Vec<Packet> {
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    let mut demuxer = ctx.containers.open_demuxer(format, input, &ctx.codecs).unwrap();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => return packets,
            Err(e) => panic!("{format}: demux: {e}"),
        }
    }
}

/// The frames of a FATE DTS-HD sample, as its `dtshd` demuxer cuts them.
fn dtshd_frames(name: &str) -> Vec<Packet> {
    let path = fate(&format!("dts/dcadec-suite/{name}.dtshd"));
    demux_all("dtshd", Box::new(std::fs::File::open(path).unwrap()))
}

/// A file in this test run's scratch directory, under the directory Cargo
/// gives integration tests; each test removes its files after use.
fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dca-streaming-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// What the player's registry makes of an input: each decoded frame's
/// pts and sample count, the PCM, the output layout and the number of
/// packets the decoder refused.
struct Decoded {
    frames: Vec<(Option<i64>, u32)>,
    pcm: Vec<u8>,
    format: Option<AudioFormat>,
    errors: usize,
}

/// Decode stream `a:0` of `path` through the player's registry: the
/// probe picks the container, the registry the decoder. Decode errors are
/// counted, as FFmpeg's command line counts and skips them.
fn decode(path: &Path) -> Decoded {
    let ctx = codecs::context();
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio).unwrap().clone();
    assert_eq!(stream.params.codec_id.as_str(), "dts", "{}: codec", path.display());
    let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
    let mut out = Decoded { frames: Vec::new(), pcm: Vec::new(), format: None, errors: 0 };
    let drain = |decoder: &mut Box<dyn Decoder>, out: &mut Decoded| loop {
        match decoder.receive_frame() {
            Ok(Frame::Audio(a)) => {
                out.frames.push((a.pts, a.samples));
                out.pcm.extend_from_slice(&a.data[0]);
            }
            Ok(_) => {}
            Err(Error::NeedMore | Error::Eof) => break,
            Err(e) => panic!("receive_frame: {e}"),
        }
    };
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == stream.index => {
                if decoder.send_packet(&p).is_err() {
                    out.errors += 1;
                }
                drain(&mut decoder, &mut out);
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{}: demux: {e}", path.display()),
        }
    }
    if decoder.flush().is_err() {
        out.errors += 1;
    }
    drain(&mut decoder, &mut out);
    out.format = decoder.output_audio_format();
    out
}

/// `ffprobe -show_frames`: each decoded frame's pts and sample count in
/// the stream time base, as FFmpeg's decoder returns them (skip-samples
/// trimming applied).
fn ffmpeg_frames(path: &Path) -> Vec<(Option<i64>, u32)> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "frame=pts,nb_samples", "-of", "compact"])
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let frames: Vec<(Option<i64>, u32)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('|');
            (fields.next()? == "frame").then(|| {
                let kv: HashMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
                (kv["pts"].parse().ok(), kv["nb_samples"].parse().unwrap())
            })
        })
        .collect();
    assert!(!frames.is_empty(), "ffprobe {}: no decoded frames", path.display());
    frames
}

/// FFmpeg's decode of `a:0` as raw interleaved little-endian `fmt`.
fn ffmpeg_pcm(path: &Path, fmt: &str) -> Vec<u8> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", fmt, "-c:a", &format!("pcm_{fmt}"), "-"])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "ffmpeg {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Our PCM against FFmpeg's in our output format: integer output must be
/// bit-exact, float output (the lossy filter banks) at least 90 dB, the
/// floor of the reference suite. Equal length in both cases.
fn assert_pcm_matches(what: &str, path: &Path, ours: &Decoded) {
    match ours.format.map(|f| f.sample_format) {
        Some(SampleFormat::S32) | Some(SampleFormat::S16) => {
            let fmt = if ours.format.unwrap().sample_format == SampleFormat::S16 { "s16le" } else { "s32le" };
            let theirs = ffmpeg_pcm(path, fmt);
            assert_eq!(ours.pcm.len(), theirs.len(), "{what}: PCM length vs FFmpeg");
            assert!(ours.pcm == theirs, "{what}: PCM differs from FFmpeg (lossless must be bit-exact)");
        }
        Some(SampleFormat::F32) => {
            let theirs = ffmpeg_pcm(path, "f32le");
            assert_eq!(ours.pcm.len(), theirs.len(), "{what}: PCM length vs FFmpeg");
            let f32s = |b: &[u8]| b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect::<Vec<_>>();
            let snr = refcheck::snr_db(&f32s(&theirs), &f32s(&ours.pcm), 0);
            assert!(snr >= 90.0, "{what}: SNR {snr:.2} dB < 90 dB vs FFmpeg");
        }
        other => panic!("{what}: unexpected output format {other:?}"),
    }
}

// ───────────────────────── MPEG-TS writer ─────────────────────────

const DTS_PID: u16 = 0x100;
const PMT_PID: u16 = 0x1000;

/// CRC-32/MPEG-2 of a PSI section.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// A PSI section, CRC appended, alone in one TS packet.
fn psi_packet(pid: u16, section: &[u8]) -> Vec<u8> {
    let mut pkt = vec![0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10, 0];
    pkt.extend_from_slice(section);
    pkt.extend_from_slice(&crc32_mpeg2(section).to_be_bytes());
    pkt.resize(188, 0xFF);
    pkt
}

/// A transport stream with one program whose only stream is DTS on
/// [`DTS_PID`] — private PES (`stream_type` 0x06) named by a `DTS1`
/// registration descriptor, as FFmpeg and the player's demuxer both read
/// it — carrying `pes`: each payload in one private-stream-1 PES with its
/// PTS, if any.
fn transport_stream(pes: &[(Vec<u8>, Option<u64>)]) -> Vec<u8> {
    let pmt_hi = 0xE0 | (PMT_PID >> 8) as u8;
    let es_hi = 0xE0 | (DTS_PID >> 8) as u8;
    let mut ts = psi_packet(0, &[0x00, 0xB0, 13, 0x00, 0x01, 0xC1, 0, 0, 0x00, 0x01, pmt_hi, PMT_PID as u8]);
    ts.extend(psi_packet(
        PMT_PID,
        &[
            0x02, 0xB0, 24, 0x00, 0x01, 0xC1, 0, 0, es_hi, DTS_PID as u8, 0xF0, 0, // program
            0x06, es_hi, DTS_PID as u8, 0xF0, 6, 0x05, 4, b'D', b'T', b'S', b'1', // stream
        ],
    ));
    let mut cc = 0u8;
    for (payload, pts) in pes {
        let mut pes = vec![0, 0, 1, 0xBD, 0, 0, 0x80, if pts.is_some() { 0x80 } else { 0 }, if pts.is_some() { 5 } else { 0 }];
        if let Some(t) = *pts {
            pes.extend_from_slice(&[
                0x21 | ((t >> 29) & 0x0E) as u8,
                (t >> 22) as u8,
                ((t >> 14) & 0xFE) as u8 | 1,
                (t >> 7) as u8,
                ((t << 1) & 0xFE) as u8 | 1,
            ]);
        }
        pes.extend_from_slice(payload);
        let len = u16::try_from(pes.len() - 6).expect("PES payload fits PES_packet_length");
        pes[4..6].copy_from_slice(&len.to_be_bytes());
        for (i, chunk) in pes.chunks(184).enumerate() {
            let pusi = if i == 0 { 0x40 } else { 0 };
            let mut pkt = vec![0x47, pusi | (DTS_PID >> 8) as u8, DTS_PID as u8];
            let stuffing = 184 - chunk.len();
            if stuffing == 0 {
                pkt.push(0x10 | cc);
            } else {
                // adaptation field of `stuffing` bytes: its length, then
                // a flags byte and 0xFF stuffing
                pkt.push(0x30 | cc);
                pkt.push((stuffing - 1) as u8);
                if stuffing > 1 {
                    pkt.push(0);
                    pkt.resize(pkt.len() + stuffing - 2, 0xFF);
                }
            }
            pkt.extend_from_slice(chunk);
            assert_eq!(pkt.len(), 188);
            ts.extend_from_slice(&pkt);
            cc = (cc + 1) & 0x0F;
        }
    }
    ts
}

/// `es` cut at `cuts` into PES payloads, each stamped as MPEG-2 systems
/// stamps a PES: the PTS of the first frame that starts in it (frame `k`
/// starts at `starts[k]` and is presented at `times[k]`), none when no
/// frame starts in it.
fn pes_cut(es: &[u8], cuts: impl IntoIterator<Item = usize>, starts: &[usize], times: &[u64]) -> Vec<(Vec<u8>, Option<u64>)> {
    let mut bounds = vec![0];
    bounds.extend(cuts.into_iter().filter(|&c| c > 0 && c < es.len()));
    bounds.push(es.len());
    bounds.sort_unstable();
    bounds.dedup();
    bounds
        .windows(2)
        .map(|w| {
            let pts = starts.iter().position(|&s| (w[0]..w[1]).contains(&s)).map(|k| times[k]);
            (es[w[0]..w[1]].to_vec(), pts)
        })
        .collect()
}

/// Where the extension substream of a core + substream frame starts.
fn substream_offset(frame: &[u8]) -> usize {
    (4..frame.len() - 4)
        .step_by(4)
        .find(|&i| frame[i..i + 4] == [0x64, 0x58, 0x20, 0x25])
        .expect("frame has an extension substream after its core")
}

/// A 90 kHz PTS for the first frame, away from zero.
const BASE: u64 = 900_000;

// ───────────────────── frames across PES packets ─────────────────────

/// The frames of `name` as one DTS elementary stream in MPEG-TS, cut
/// every way a muxer may cut it; one frame per PES is the control. Each
/// cut decodes to FFmpeg's PCM and frame sizes, every frame at its own
/// presentation time: a PES's PTS belongs to the first frame that starts
/// in it (ISO/IEC 13818-1 2.4.3.7), the others follow it. FFmpeg's
/// timestamps are that timeline, except where a PES boundary splits a
/// sync word: its parser then fetches a frame's PTS from the PES in which
/// it saw the frame before end, the next one, and runs a frame late.
fn repacketized_like_ffmpeg(name: &str) {
    let frames = dtshd_frames(name);
    let rate = frames[0].time_base.den();
    let mut es = Vec::new();
    let (mut starts, mut exss, mut times) = (Vec::new(), Vec::new(), Vec::new());
    let mut t = BASE;
    for f in &frames {
        starts.push(es.len());
        exss.push(es.len() + substream_offset(&f.data));
        times.push(t);
        es.extend_from_slice(&f.data);
        // packet durations are in 1/rate: rescale to 90 kHz exactly
        let d = u64::try_from(f.duration.unwrap()).unwrap() * 90_000;
        assert_eq!(d % rate as u64, 0, "{name}: frame duration is a whole number of 90 kHz ticks");
        t += d / rate as u64;
    }
    let n = frames.len();
    let cuts: Vec<(&str, Vec<usize>)> = vec![
        ("one frame per PES", starts.clone()),
        ("cut inside the sync word", starts.iter().map(|s| s + 2).collect()),
        ("cut between core and substream", exss.clone()),
        ("cut inside the substream header", exss.iter().map(|e| e + 5).collect()),
        ("1000-byte PES", (1..).map(|i| i * 1000).take_while(|&c| c < es.len()).collect()),
        ("three frames per PES, cut in a substream", (2..n).step_by(3).map(|k| exss[k] + 9).collect()),
    ];
    let mut failures = Vec::new();
    for (i, (what, cuts)) in cuts.into_iter().enumerate() {
        let pes = pes_cut(&es, cuts, &starts, &times);
        let path = scratch(&format!("{name}-{i}.ts"), &transport_stream(&pes));
        let ours = decode(&path);
        let theirs = ffmpeg_frames(&path);
        let timeline: Vec<(Option<i64>, u32)> = times.iter().zip(&theirs).map(|(&t, &(_, n))| (Some(t as i64), n)).collect();
        if theirs.len() != n {
            failures.push(format!("{what}: FFmpeg decoded {} of {n} frames", theirs.len()));
        } else if what != "cut inside the sync word" && theirs != timeline {
            failures.push(format!("{what}: FFmpeg's frames {theirs:?} are not the timeline {timeline:?}"));
        } else if ours.errors != 0 {
            failures.push(format!("{what}: {} decode errors", ours.errors));
        } else if ours.frames != timeline {
            failures.push(format!("{what}: frames (pts, samples) {:?}, timeline {timeline:?}", ours.frames));
        } else {
            assert_pcm_matches(&format!("{name}, {what}"), &path, &ours);
        }
        let _ = std::fs::remove_file(&path);
    }
    assert!(failures.is_empty(), "{name}: {} cuts differ:\n{}", failures.len(), failures.join("\n"));
}

/// DTS-HD Master Audio: core plus an XLL substream, lossless.
#[test]
fn xll_frames_split_across_pes_packets_decode_like_ffmpeg() {
    repacketized_like_ffmpeg("xll_51_24_48_768");
}

/// DTS-HD High Resolution: core plus an XBR substream, lossy.
#[test]
fn xbr_frames_split_across_pes_packets_decode_like_ffmpeg() {
    repacketized_like_ffmpeg("xbr_51_24_48_3840");
}

/// The decoder holds a frame until the next one starts or the input ends:
/// fed the stream in two packets cut inside a substream, it emits the
/// same frames and timestamps as fed one frame per packet, the last one
/// only at the end of the input. Reset forgets a frame in progress.
#[test]
fn decoder_completes_frames_across_packets_until_eof_and_reset_forgets_them() {
    let frames = dtshd_frames("xll_51_24_48_768");
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    // no DTS-HD padding options: every decoded sample is output
    let params = CodecParameters::audio(CodecId::new("dts"));
    let new_decoder = || ctx.codecs.first_decoder(&params).unwrap();
    let run = |decoder: &mut Box<dyn Decoder>, packets: &[Packet], out: &mut Vec<(Option<i64>, Vec<u8>)>| {
        for p in packets {
            decoder.send_packet(p).unwrap();
            while let Ok(Frame::Audio(a)) = decoder.receive_frame() {
                out.push((a.pts, a.data[0].clone()));
            }
        }
    };
    let finish = |decoder: &mut Box<dyn Decoder>, out: &mut Vec<(Option<i64>, Vec<u8>)>| {
        decoder.flush().unwrap();
        while let Ok(Frame::Audio(a)) = decoder.receive_frame() {
            out.push((a.pts, a.data[0].clone()));
        }
    };

    let mut reference = Vec::new();
    let mut decoder = new_decoder();
    run(&mut decoder, &frames, &mut reference);
    finish(&mut decoder, &mut reference);
    assert!(!reference.is_empty());

    // Two packets, cut inside frame 3's substream; the second packet
    // starts no frame, so it has no timestamp.
    let mut es = Vec::new();
    let mut cut = 0;
    for (k, f) in frames.iter().enumerate() {
        if k == 3 {
            cut = es.len() + substream_offset(&f.data) + 11;
        }
        es.extend_from_slice(&f.data);
    }
    let tb = frames[0].time_base;
    let halves = [
        Packet::new(0, tb, es[..cut].to_vec()).with_pts(frames[0].pts.unwrap()),
        Packet::new(0, tb, es[cut..].to_vec()),
    ];
    let mut decoder = new_decoder();
    let mut got = Vec::new();
    run(&mut decoder, &halves, &mut got);
    let before_eof = got.len();
    finish(&mut decoder, &mut got);
    assert_eq!(got, reference, "two packets cut inside a substream vs one frame per packet");
    assert!(before_eof < got.len(), "the last frame waits for the end of the input");

    // A frame in progress, then reset: the stream decodes from scratch.
    let mut decoder = new_decoder();
    let mut discarded = Vec::new();
    run(&mut decoder, &frames[..3], &mut discarded);
    let partial = &frames[3].data[..frames[3].data.len() / 2];
    run(&mut decoder, &[Packet::new(0, tb, partial.to_vec())], &mut discarded);
    decoder.reset().unwrap();
    let mut after_reset = Vec::new();
    run(&mut decoder, &frames, &mut after_reset);
    finish(&mut decoder, &mut after_reset);
    assert_eq!(after_reset, reference, "decoding after reset vs a fresh decoder");
}

/// A frame takes the PTS of the packet its first byte arrived in, however
/// many packets its sync marker spans before the frame is recognised (a
/// core marker on its sixth byte): its first six bytes come one per
/// packet, the PTS on the first only, then the rest. As the first frame,
/// and as a frame after a timestamp jump, where following on from the
/// frame before would give another time.
#[test]
fn a_frame_keeps_the_pts_of_its_first_byte_across_one_byte_packets() {
    let frames = demux_all("dts", Box::new(std::fs::File::open(fate("dts/dts_es.dts")).unwrap()));
    let (first, second) = (&frames[0].data, &frames[1].data);
    let tb = TimeBase::new(1, 90_000);
    let dribble = |frame: &[u8], pts: i64| -> Vec<Packet> {
        let mut packets: Vec<Packet> = frame[..6].iter().map(|&b| Packet::new(0, tb, vec![b])).collect();
        packets[0] = packets[0].clone().with_pts(pts);
        packets.push(Packet::new(0, tb, frame[6..].to_vec()));
        packets
    };
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    let params = CodecParameters::audio(CodecId::new("dts"));
    for (what, packets, want) in [
        ("first frame", dribble(first, 9000), vec![Some(9000)]),
        (
            "frame after a jump",
            [vec![Packet::new(0, tb, first.clone()).with_pts(9000)], dribble(second, 30_000)].concat(),
            vec![Some(9000), Some(30_000)],
        ),
    ] {
        let mut decoder = ctx.codecs.first_decoder(&params).unwrap();
        for p in &packets {
            decoder.send_packet(p).unwrap_or_else(|e| panic!("{what}: {e}"));
        }
        decoder.flush().unwrap();
        let mut times = Vec::new();
        while let Ok(Frame::Audio(a)) = decoder.receive_frame() {
            times.push(a.pts);
        }
        assert_eq!(times, want, "{what}: frame timestamps");
    }
}

// ───────────────────── timing past a frame that fails ─────────────────────

/// Flip the primary channel count that follows a core frame header: the
/// header, and with it the frame's size and duration, stays intact, but
/// the frame no longer decodes (FFmpeg: "Invalid number of primary audio
/// channels").
fn break_channel_count(frame: &mut [u8]) {
    let crc_present = frame[4] & 0x02 != 0;
    let at = if crc_present { 120 } else { 104 } + 4;
    for bit in at..at + 3 {
        frame[bit / 8] ^= 0x80 >> (bit % 8);
    }
}

/// Six core frames in one PES, the third undecodable: FFmpeg's parser
/// still times it, so the frames after it keep their own times. Frames,
/// timestamps and PCM equal FFmpeg's.
#[test]
fn a_frame_that_fails_to_decode_keeps_its_place_on_the_timeline() {
    let frames = dtshd_frames("core_51_24_48_768_0");
    let mut es = Vec::new();
    for (k, f) in frames.iter().enumerate() {
        let mut data = f.data.clone();
        if k == 2 {
            break_channel_count(&mut data);
        }
        es.extend_from_slice(&data);
    }
    let path = scratch("bad-third-frame.ts", &transport_stream(&[(es, Some(BASE))]));
    let ours = decode(&path);
    let theirs = ffmpeg_frames(&path);
    assert_eq!(theirs.len(), frames.len() - 1, "FFmpeg drops exactly the broken frame");
    assert_eq!(ours.frames, theirs, "decoded frames (pts, samples) vs FFmpeg");
    assert_pcm_matches("one undecodable frame", &path, &ours);
    let _ = std::fs::remove_file(&path);
}

/// A frame whose header does not parse (sampling frequency code 0) has no
/// duration: the frames after it in the packet have no known time, and
/// get none instead of the broken frame's slot.
#[test]
fn a_frame_without_duration_leaves_the_frames_after_it_untimed() {
    let frames = dtshd_frames("core_51_24_48_768_0");
    let mut es = Vec::new();
    for (k, f) in frames.iter().enumerate() {
        let mut data = f.data.clone();
        if k == 2 {
            // sfreq: bits 66..70 of the core frame header
            data[8] &= !0x3C;
        }
        es.extend_from_slice(&data);
    }
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    let mut decoder = ctx.codecs.first_decoder(&CodecParameters::audio(CodecId::new("dts"))).unwrap();
    let tb = TimeBase::new(1, 90_000);
    let pts = BASE as i64;
    let _ = decoder.send_packet(&Packet::new(0, tb, es).with_pts(pts));
    let mut times = Vec::new();
    let mut drain = |decoder: &mut Box<dyn Decoder>| {
        while let Ok(Frame::Audio(a)) = decoder.receive_frame() {
            assert_eq!(a.samples, 512);
            times.push(a.pts);
        }
    };
    drain(&mut decoder);
    let _ = decoder.flush();
    drain(&mut decoder);
    assert_eq!(times, [Some(pts), Some(pts + 960), None, None, None], "frames after an undated frame");
}

// ───────────────────── DTS-HD skip-samples timestamps ─────────────────────

/// Every DTS-HD input of FFmpeg's dca.mak.
const DTSHD_SUITE: [&str; 19] = [
    "xll_51_16_192_768_0",
    "xll_51_16_192_768_1",
    "xll_51_24_48_768",
    "xll_51_24_48_none",
    "xll_71_24_48_768_0",
    "xll_71_24_48_768_1",
    "xll_71_24_96_768",
    "xll_x96_51_24_96_1509",
    "xll_xch_61_24_48_768",
    "core_51_24_48_768_0",
    "core_51_24_48_768_1",
    "x96_51_24_96_1509",
    "x96_xch_61_24_96_3840",
    "x96_xxch_71_24_96_3840",
    "xbr_51_24_48_3840",
    "xbr_xch_61_24_48_3840",
    "xbr_xxch_71_24_48_3840",
    "xch_61_24_48_768",
    "xxch_71_24_48_2046",
];

/// FFmpeg trims a DTS-HD file's initial padding from the decoded output
/// and moves a frame's pts by the samples trimmed from its head, only
/// that frame's. Every suite input pads two whole frames (one for
/// `xll_51_24_48_none`); copies of two of them pad 100 samples (inside
/// the first frame), 700 (the second frame) and 1500 (the third). Each
/// decoded frame's pts and sample count equals FFmpeg's, and the copies'
/// PCM too.
#[test]
fn dtshd_decoded_frames_carry_ffmpegs_trimmed_timestamps() {
    let mut cases: Vec<(String, PathBuf, bool)> =
        DTSHD_SUITE.iter().map(|n| (n.to_string(), fate(&format!("dts/dcadec-suite/{n}.dtshd")), false)).collect();
    for (name, padding) in [("xll_51_24_48_768", 100u16), ("xll_51_24_48_768", 700), ("core_51_24_48_768_0", 1500)] {
        let mut bytes = std::fs::read(fate(&format!("dts/dcadec-suite/{name}.dtshd"))).unwrap();
        let aupr = bytes.windows(8).position(|w| w == b"AUPR-HDR").unwrap();
        // AUPR_HDR body: ..., channel mask (2), initial padding (2) at 19
        bytes[aupr + 16 + 19..aupr + 16 + 21].copy_from_slice(&padding.to_be_bytes());
        let path = scratch(&format!("{name}-padding-{padding}.dtshd"), &bytes);
        cases.push((format!("{name} with {padding} samples of padding"), path, true));
    }
    let mut failures = Vec::new();
    for (name, path, compare_pcm) in &cases {
        let ours = decode(path);
        let theirs = ffmpeg_frames(path);
        if ours.frames != theirs {
            failures.push(format!("{name}: ours {:?}\n  FFmpeg {:?}", ours.frames, theirs));
        } else if *compare_pcm {
            assert_pcm_matches(name, path, &ours);
        }
        if *compare_pcm {
            let _ = std::fs::remove_file(path);
        }
    }
    assert!(failures.is_empty(), "{} of {} inputs differ from FFmpeg's decoded frames:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

// ───────────────────── reassembly bounds ─────────────────────

/// A reader that counts the bytes it hands out.
struct Counted {
    inner: std::io::Cursor<Vec<u8>>,
    read: Arc<AtomicU64>,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Counted {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Two real core frames from dts_es.dts, the first padded with zeros (no
/// marker) to `len` bytes, and the first one's duration (ticks of its
/// demuxer's time base) and time base denominator.
fn padded_frames(len: usize) -> (Vec<u8>, Vec<u8>, i64, i64) {
    let packets = demux_all("dts", Box::new(std::fs::File::open(fate("dts/dts_es.dts")).unwrap()));
    let mut first = packets[0].data.clone();
    assert!(first.len() < len);
    first.resize(len, 0);
    (first, packets[1].data.clone(), packets[0].duration.unwrap(), packets[0].time_base.den())
}

/// A DTS-HD file around `es`: DTSHDHDR, AUPR-HDR (48 kHz, no padding),
/// STRMDATA.
fn dtshd_file(es: &[u8]) -> Vec<u8> {
    let mut f = b"DTSHDHDR".to_vec();
    f.extend_from_slice(&4u64.to_be_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(b"AUPR-HDR");
    f.extend_from_slice(&21u64.to_be_bytes());
    let mut aupr = [0u8; 21];
    aupr[3..6].copy_from_slice(&48_000u32.to_be_bytes()[1..]);
    aupr[6..10].copy_from_slice(&2u32.to_be_bytes());
    aupr[10..12].copy_from_slice(&512u16.to_be_bytes());
    aupr[17..19].copy_from_slice(&0x000Fu16.to_be_bytes());
    f.extend_from_slice(&aupr);
    f.extend_from_slice(b"STRMDATA");
    f.extend_from_slice(&(es.len() as u64).to_be_bytes());
    f.extend_from_slice(es);
    f
}

/// Both demuxers cut a frame of exactly the largest size the decoder
/// takes. One byte more is an error; so is a frame that never ends,
/// raised once that much is held and long before the rest is read. The
/// next frame still comes out after either, timed after the frame before
/// it as FFmpeg times the oversized packet it returns.
#[test]
fn demuxers_hold_at_most_the_largest_decodable_frame() {
    for (format, wrap) in [("dts", false), ("dtshd", true)] {
        for extra in [0usize, 1, 4 << 20] {
            let (first, second, first_duration, _) = padded_frames(MAX_PACKET_SIZE + extra);
            let mut es = first.clone();
            es.extend_from_slice(&second);
            let bytes = if wrap { dtshd_file(&es) } else { es };
            let read = Arc::new(AtomicU64::new(0));
            let input = Counted { inner: std::io::Cursor::new(bytes), read: read.clone() };
            let mut ctx = RuntimeContext::new();
            codec_dca::register(&mut ctx);
            let mut demuxer = ctx.containers.open_demuxer(format, Box::new(input), &ctx.codecs).unwrap();
            let what = format!("{format}: frame of {} bytes", first.len());
            if extra == 0 {
                let p = demuxer.next_packet().unwrap_or_else(|e| panic!("{what}: {e}"));
                assert!(p.data == first, "{what}: first packet of {} bytes", p.data.len());
            } else {
                match demuxer.next_packet() {
                    Err(Error::InvalidData(_)) => {}
                    other => panic!("{what}: expected an oversized-frame error, got {:?}", other.map(|p| p.data.len())),
                }
                let held = read.load(Ordering::Relaxed);
                if extra > 1 {
                    assert!(held <= (MAX_PACKET_SIZE + 300 * 1024) as u64, "{what}: read {held} bytes before failing");
                }
            }
            let p = demuxer.next_packet().unwrap_or_else(|e| panic!("{what}: after the first frame: {e}"));
            assert!(p.data == second, "{what}: the frame after it ({} bytes)", p.data.len());
            assert_eq!(p.pts, Some(first_duration), "{what}: the frame after it starts where the first ends");
            assert!(matches!(demuxer.next_packet(), Err(Error::Eof)), "{what}: end");
        }
    }
}

/// The last frame of the input, with no marker after it, comes out of the
/// parser flush. At the largest size the decoder takes it is a packet; up
/// to five bytes longer (all a demuxer holds while no marker says the
/// frame ended) it is the oversized-frame error a marker-ended frame
/// gives, and then the end of the input.
#[test]
fn demuxers_hold_the_last_frame_to_the_largest_decodable_size() {
    for (format, wrap) in [("dts", false), ("dtshd", true)] {
        for extra in 0..=5usize {
            let (frame, _, _, _) = padded_frames(MAX_PACKET_SIZE + extra);
            let bytes = if wrap { dtshd_file(&frame) } else { frame.clone() };
            let mut ctx = RuntimeContext::new();
            codec_dca::register(&mut ctx);
            let mut demuxer = ctx.containers.open_demuxer(format, Box::new(std::io::Cursor::new(bytes)), &ctx.codecs).unwrap();
            let what = format!("{format}: last frame of {} bytes", frame.len());
            if extra == 0 {
                let p = demuxer.next_packet().unwrap_or_else(|e| panic!("{what}: {e}"));
                assert!(p.data == frame, "{what}: packet of {} bytes", p.data.len());
            } else {
                match demuxer.next_packet() {
                    Err(Error::InvalidData(_)) => {}
                    other => panic!("{what}: expected an oversized-frame error, got {:?}", other.map(|p| p.data.len())),
                }
            }
            assert!(matches!(demuxer.next_packet(), Err(Error::Eof)), "{what}: end");
        }
    }
}

/// The decoder's reassembly is bounded the same way: a frame one byte
/// over the largest decodable size, or one that never ends, costs one
/// error and its bytes are dropped. The frames after it decode at their
/// packets' timestamps, or without them after the dropped frame, which
/// keeps its place on the timeline.
#[test]
fn decoder_holds_at_most_the_largest_decodable_frame() {
    let third = demux_all("dts", Box::new(std::fs::File::open(fate("dts/dts_es.dts")).unwrap()))[2].data.clone();
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    for extra in [1usize, 4 << 20] {
        for packet_times in [true, false] {
            let (first, second, duration, rate) = padded_frames(MAX_PACKET_SIZE + extra);
            let tick = duration * 90_000 / rate;
            let mut decoder = ctx.codecs.first_decoder(&CodecParameters::audio(CodecId::new("dts"))).unwrap();
            let tb = TimeBase::new(1, 90_000);
            let mut errors = 0;
            for (n, chunk) in first.chunks(64 * 1024).enumerate() {
                let packet = Packet::new(0, tb, chunk.to_vec());
                let packet = if n == 0 { packet.with_pts(9000) } else { packet };
                if decoder.send_packet(&packet).is_err() {
                    errors += 1;
                }
                assert!(decoder.receive_frame().is_err(), "{extra}: no frame from an oversized one");
            }
            for (k, data) in [(1, second.clone()), (2, third.clone())] {
                let packet = Packet::new(0, tb, data);
                let packet = if packet_times { packet.with_pts(20_000 + 960 * (k - 1)) } else { packet };
                if decoder.send_packet(&packet).is_err() {
                    errors += 1;
                }
            }
            if decoder.flush().is_err() {
                errors += 1;
            }
            let mut times = Vec::new();
            while let Ok(Frame::Audio(a)) = decoder.receive_frame() {
                times.push(a.pts);
            }
            let want = if packet_times { [Some(20_000), Some(20_960)] } else { [Some(9000 + tick), Some(9000 + 2 * tick)] };
            assert_eq!(errors, 1, "{extra}: one error for the oversized frame");
            assert_eq!(times, want, "{extra}: the frames after it, packet timestamps {packet_times}");
        }
    }
}
