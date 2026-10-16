//! The audio bridge (AetherEngine's, reimplemented): a track AVPlayer cannot
//! take from fMP4 is decoded with the engine's decoders and served as FLAC.
//! The FLAC blocks lie on one grid on the item axis, block n covering
//! samples `[n * BLOCK, (n + 1) * BLOCK)` with frame number n, and a segment
//! holds the blocks that start in it, so the segments of one producer run
//! meet without a gap or an overlap. A run started after a seek places its
//! audio by the container's timestamps: Matroska's whole milliseconds put
//! it up to half a millisecond from where a run from the start has it when
//! frames do not end on whole milliseconds (TrueHD, DTS, Vorbis, FLAC; not
//! MP3, Opus or AC-3 at 48 kHz). Only a segment played next to one from
//! another run shows that, as a jump of that size.
//!
//! The frames are VERBATIM: the samples as they are, 16 or 24 bits. Over
//! 127.0.0.1 compression buys nothing, and OxideAV's FLAC encoder took
//! 2.5 s per 4 s segment of 7.1 (measured), more than real time allows.

use std::sync::Arc;

use oxideav_core::{AudioTrim, Packet};
use oxideav_flac::crc::{crc16, crc8};

/// Samples per FLAC block, and the grid step.
pub(crate) const BLOCK: i64 = 4096;
/// Decoded output within this of where the previous output ended continues
/// it (the decode route's `AUDIO_ALIGN_SECS`).
const ALIGN_SECS: f64 = 0.040;
/// A restarted segment decodes from this long before its first sample:
/// MP3's bit reservoir, overlapped transforms, TrueHD's restart points.
pub(crate) const PREROLL_SECS: f64 = 1.0;
/// The most silence a gap in the decoded audio gets (`plan::trusted` allows
/// keyframes 30 s apart); a stamp further ahead is taken as damage and the
/// audio continues where it was.
const MAX_GAP_SECS: f64 = 30.0;

/// Decoded audio as the engine's decoders give it: interleaved f32 in the
/// decoder's channel order, starting at `pts` (source seconds). `begins`:
/// the first sample after a declared start skip.
pub(crate) struct Pcm {
    pub samples: Vec<f32>,
    pub channels: usize,
    pub rate: u32,
    pub pts: f64,
    pub begins: bool,
}

/// The engine's decoder for one audio stream, with its container trims.
pub(crate) trait AudioDecode: Send {
    /// Decodes one packet and appends what comes out, trimmed. `Err`: the
    /// decoder refused the packet.
    fn packet(&mut self, packet: &Packet, trim: Option<AudioTrim>, out: &mut Vec<Pcm>) -> Result<(), String>;
    /// The stream ended: the decoder's tail and what the trims held back.
    fn finish(&mut self, out: &mut Vec<Pcm>);
}

pub(crate) type DecoderFactory = Arc<dyn Fn() -> Result<Box<dyn AudioDecode>, String> + Send + Sync>;

/// What a bridged track becomes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Output {
    pub rate: u32,
    pub channels: usize,
    /// 16 or 24.
    pub bits: u16,
    /// The source's codec id: its decoder's channel order.
    pub codec_id: String,
}

/// 24 bits for lossless sources, 16 for the rest (a lossy decode gains
/// nothing from more). DTS-HD Master Audio is lossless and shares the
/// `dts` id with lossy DTS.
pub(crate) fn bits_for(codec_id: &str) -> u16 {
    let lossless = matches!(codec_id, "truehd" | "mlp" | "flac" | "alac" | "wavpack" | "tta" | "ape" | "dts") || codec_id.starts_with("pcm_");
    if lossless { 24 } else { 16 }
}

/// The STREAMINFO metadata block for `output`: fixed blocking at `BLOCK`;
/// frame sizes, length and MD5 unknown.
pub(crate) fn streaminfo(output: &Output) -> Vec<u8> {
    let mut block = Vec::with_capacity(38);
    // Last metadata block, STREAMINFO, 34 bytes.
    block.extend_from_slice(&[0x80, 0, 0, 34]);
    block.extend_from_slice(&(BLOCK as u16).to_be_bytes());
    block.extend_from_slice(&(BLOCK as u16).to_be_bytes());
    block.extend_from_slice(&[0; 6]);
    let format = (u64::from(output.rate) << 44) | ((output.channels as u64 - 1) << 41) | (u64::from(output.bits - 1) << 36);
    block.extend_from_slice(&format.to_be_bytes());
    block.extend_from_slice(&[0; 16]);
    block
}

/// FLAC's channel order is WAVE's, which the engine's decoders output, but
/// the Vorbis decoder keeps the bitstream's order. `out[i] = in[map[i]]`.
fn channel_map(codec_id: &str, channels: usize) -> Option<&'static [usize]> {
    if codec_id != "vorbis" {
        return None;
    }
    match channels {
        3 => Some(&[0, 2, 1]),
        5 => Some(&[0, 2, 1, 3, 4]),
        6 => Some(&[0, 2, 1, 5, 3, 4]),
        7 => Some(&[0, 2, 1, 6, 5, 3, 4]),
        8 => Some(&[0, 2, 1, 7, 5, 6, 3, 4]),
        _ => None,
    }
}

/// FLAC frame `number` holding `samples` (interleaved, `output.channels`
/// wide): one VERBATIM subframe per channel.
fn frame(output: &Output, number: u64, samples: &[f32]) -> Vec<u8> {
    let channels = output.channels;
    let frames = samples.len() / channels;
    let width = usize::from(output.bits / 8);
    let mut out = Vec::with_capacity(16 + channels * (1 + frames * width) + 2);
    // Sync, fixed blocking.
    out.extend_from_slice(&[0xFF, 0xF8]);
    // Block size: 4096 has a code; the last, short block follows the
    // number as 16 bits, minus one.
    let size_code: u8 = if frames == BLOCK as usize { 12 } else { 7 };
    let rate_code: u8 = match output.rate {
        88_200 => 1,
        176_400 => 2,
        192_000 => 3,
        8_000 => 4,
        16_000 => 5,
        22_050 => 6,
        24_000 => 7,
        32_000 => 8,
        44_100 => 9,
        48_000 => 10,
        96_000 => 11,
        // From STREAMINFO.
        _ => 0,
    };
    out.push(size_code << 4 | rate_code);
    // Independent channels; 16 or 24 bits.
    let size_bits: u8 = if output.bits == 24 { 6 } else { 4 };
    out.push(((channels as u8 - 1) << 4) | (size_bits << 1));
    write_utf8(number, &mut out);
    if size_code == 7 {
        out.extend_from_slice(&((frames - 1) as u16).to_be_bytes());
    }
    out.push(crc8(&out));
    // The decoders' scale (2^(bits-1)), so 24-bit sources come back exact.
    let scale = f64::from(1u32 << (output.bits - 1));
    let (low, high) = (-scale, scale - 1.0);
    for c in 0..channels {
        // VERBATIM, no wasted bits.
        out.push(0x02);
        for frame in samples.chunks_exact(channels) {
            let v = (f64::from(frame[c]) * scale).round().clamp(low, high) as i32;
            out.extend_from_slice(&v.to_be_bytes()[4 - width..]);
        }
    }
    let crc = crc16(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// FLAC's UTF-8 coding of a frame number (up to 36 bits, 7 bytes).
fn write_utf8(value: u64, out: &mut Vec<u8>) {
    if value < 0x80 {
        out.push(value as u8);
        return;
    }
    let len = [0x800u64, 0x1_0000, 0x20_0000, 0x400_0000, 0x8000_0000].iter().position(|&limit| value < limit).map_or(7, |i| i as u32 + 2);
    let prefix = !(0xFFu32 >> len) as u8;
    let payload = u64::from(0xFFu32 >> (len + 1));
    out.push(prefix | ((value >> (6 * (len - 1))) & payload) as u8);
    for i in (0..len - 1).rev() {
        out.push(0x80 | ((value >> (6 * i)) & 0x3F) as u8);
    }
}

/// One FLAC frame and the item sample it starts at.
pub(crate) struct Block {
    pub at: i64,
    pub samples: u32,
    pub data: Vec<u8>,
}

pub(crate) struct Bridge {
    decoder: Box<dyn AudioDecode>,
    output: Output,
    map: Option<&'static [usize]>,
    /// Source seconds of item sample 0.
    origin: f64,
    /// Decoded samples not yet framed, interleaved in FLAC order; the first
    /// is item sample `buf_start`.
    buf: Vec<f32>,
    buf_start: i64,
    /// Where the next decoded sample goes, once something was decoded.
    next: Option<i64>,
    /// The decoder's tail is out: nothing more comes.
    finished: bool,
    /// Logged once: a packet the decoder refused, a rate change, a jump
    /// taken as damage.
    refused: bool,
    rate_changed: bool,
    jumped: bool,
}

impl Bridge {
    /// A bridge whose first block starts at item sample `start` (on the
    /// grid): nothing decoded before it is kept, silence fills up to the
    /// first decoded sample after it.
    pub(crate) fn new(factory: &DecoderFactory, output: Output, origin: f64, start: i64) -> Result<Bridge, String> {
        debug_assert_eq!(start.rem_euclid(BLOCK), 0);
        Ok(Bridge {
            decoder: factory()?,
            map: channel_map(&output.codec_id, output.channels),
            output,
            origin,
            buf: Vec::new(),
            buf_start: start,
            next: None,
            finished: false,
            refused: false,
            rate_changed: false,
            jumped: false,
        })
    }

    /// Item sample where what has been decoded ends.
    pub(crate) fn decoded_to(&self) -> i64 {
        self.buf_start + (self.buf.len() / self.output.channels) as i64
    }

    /// `MAX_GAP_SECS` in output samples.
    fn max_gap(&self) -> i64 {
        (MAX_GAP_SECS * f64::from(self.output.rate)) as i64
    }

    /// Decodes `packet` into the buffer. A packet the decoder refuses leaves
    /// silence as long as the container says it lasts; without a duration,
    /// what follows closes up behind it (within `ALIGN_SECS`, the decode
    /// route's rule). After a restart TrueHD refuses what comes before its
    /// next major sync, before any output.
    pub(crate) fn feed(&mut self, packet: &Packet, trim: Option<AudioTrim>) {
        if self.finished {
            return;
        }
        let mut out = Vec::new();
        if let Err(e) = self.decoder.packet(packet, trim, &mut out) {
            if !self.refused {
                self.refused = true;
                log(&format!("the audio decoder refused a packet ({e}): silence there"));
            }
            if let (Some(next), Some(duration)) = (self.next, packet.duration.filter(|&d| d > 0)) {
                let span = packet.time_base.seconds_of(duration) * f64::from(self.output.rate);
                if span.is_finite() && span > 0.0 {
                    // Never further than a gap may reach past the buffer's
                    // end: a damaged duration, or many, cannot grow it.
                    let limit = self.decoded_to().saturating_add(self.max_gap());
                    self.next = Some(next.saturating_add(span.round() as i64).min(limit));
                }
            }
        }
        for pcm in out {
            self.append(pcm);
        }
    }

    /// The stream ended: the decoder's tail joins the buffer.
    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut out = Vec::new();
        self.decoder.finish(&mut out);
        for pcm in out {
            self.append(pcm);
        }
    }

    fn append(&mut self, pcm: Pcm) {
        let rate = self.output.rate;
        let frames = pcm.samples.len() / pcm.channels.max(1);
        if frames == 0 {
            return;
        }
        // The decode route's rule: output stamped before source time 0 is
        // priming and goes, unless a declared skip just came off it.
        let mut skip = 0usize;
        let mut pts = pcm.pts;
        if pts < 0.0 {
            if !pcm.begins {
                skip = (-pts * f64::from(pcm.rate)).round() as usize;
            }
            pts = 0.0;
        }
        if skip >= frames {
            return;
        }
        let mut at = ((pts - self.origin) * f64::from(rate)).round() as i64;
        // The silence a gap gets is counted from the buffer's end, where
        // `write` fills from, whatever `next` says.
        let end = self.decoded_to();
        let limit = end.saturating_add(self.max_gap());
        let expected = self.next.unwrap_or(end).min(limit);
        if at.saturating_sub(expected) > self.max_gap() || at > limit {
            if !self.jumped {
                self.jumped = true;
                log(&format!("the audio's timestamps jump {:.1} s ahead: taken as damage", at.saturating_sub(expected) as f64 / f64::from(rate)));
            }
            at = expected;
        }
        if pcm.rate != rate {
            if !self.rate_changed {
                self.rate_changed = true;
                log(&format!("the audio's rate changed from {rate} to {} Hz: silence there", pcm.rate));
            }
            let span = ((frames - skip) as f64 * f64::from(rate) / f64::from(pcm.rate)).round() as usize;
            self.write(at, &[], 0, span);
            return;
        }
        if let Some(next) = self.next {
            if (at - next).abs() <= (ALIGN_SECS * f64::from(rate)) as i64 {
                at = next;
            } else if at < next {
                // Output that goes back over what is already there.
                skip += (next - at) as usize;
                at = next;
                if skip >= frames {
                    return;
                }
            }
        }
        let count = frames - skip;
        self.write(at, &pcm.samples[skip * pcm.channels..], pcm.channels, count);
    }

    /// Puts `count` frames (interleaved, `channels` wide; silence when
    /// `samples` is empty) at item sample `at`. Silence fills a gap before
    /// them; what falls before the buffer's start is dropped.
    fn write(&mut self, mut at: i64, mut samples: &[f32], channels: usize, mut count: usize) {
        let out_ch = self.output.channels;
        let end = self.decoded_to();
        if at < end {
            let drop = (end - at) as usize;
            self.next = Some(self.next.map_or(at + count as i64, |n| n.max(at + count as i64)));
            if drop >= count {
                return;
            }
            if !samples.is_empty() {
                samples = &samples[drop * channels..];
            }
            count -= drop;
            at = end;
        }
        self.buf.resize(self.buf.len() + (at - end) as usize * out_ch, 0.0);
        if samples.is_empty() {
            self.buf.resize(self.buf.len() + count * out_ch, 0.0);
        } else {
            self.buf.reserve(count * out_ch);
            for frame in samples.chunks_exact(channels).take(count) {
                for c in 0..out_ch {
                    let from = match self.map {
                        Some(map) if channels == out_ch => map[c],
                        _ => c,
                    };
                    self.buf.push(frame.get(from).copied().unwrap_or(0.0));
                }
            }
        }
        self.next = Some(at + count as i64);
    }

    /// The blocks from the buffer's start up to item sample `to` (on the
    /// grid); what was not decoded by then is silence.
    pub(crate) fn blocks(&mut self, to: i64) -> Vec<Block> {
        let frames = (to - self.buf_start).max(0) as usize;
        self.take(frames)
    }

    /// The blocks left up to item sample `end` (the last picture's end); the
    /// last is short.
    pub(crate) fn tail(&mut self, end: i64) -> Vec<Block> {
        let frames = (self.buf.len() / self.output.channels).min((end - self.buf_start).max(0) as usize);
        self.take(frames)
    }

    fn take(&mut self, frames: usize) -> Vec<Block> {
        let ch = self.output.channels;
        if self.buf.len() < frames * ch {
            self.buf.resize(frames * ch, 0.0);
        }
        let mut blocks = Vec::with_capacity(frames.div_ceil(BLOCK as usize));
        let mut at = self.buf_start;
        for samples in self.buf[..frames * ch].chunks(BLOCK as usize * ch) {
            let count = samples.len() / ch;
            blocks.push(Block { at, samples: count as u32, data: frame(&self.output, (at / BLOCK) as u64, samples) });
            at += count as i64;
        }
        self.buf.drain(..frames * ch);
        self.buf_start = at;
        blocks
    }
}

fn log(message: &str) {
    eprintln!("[peartube-native] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_flac::decoder::decode_frame_channels;
    use oxideav_flac::frame::parse_frame_header;
    use oxideav_flac::metadata::StreamInfo;

    /// A decoder that turns each packet into `frames` samples of one value
    /// per channel, stamped with the packet's pts; it refuses a packet whose
    /// value is 0.
    struct Steady {
        frames: usize,
        channels: usize,
    }

    impl AudioDecode for Steady {
        fn packet(&mut self, packet: &Packet, _: Option<AudioTrim>, out: &mut Vec<Pcm>) -> Result<(), String> {
            if packet.data[0] == 0 {
                return Err("refused".into());
            }
            let pts = packet.time_base.seconds_of(packet.pts.unwrap());
            let value = packet.data[0] as f32 / 256.0;
            out.push(Pcm { samples: vec![value; self.frames * self.channels], channels: self.channels, rate: 48_000, pts, begins: false });
            Ok(())
        }
        fn finish(&mut self, _: &mut Vec<Pcm>) {}
    }

    fn bridge(channels: usize, origin: f64, start: i64) -> Bridge {
        let factory: DecoderFactory = Arc::new(move || Ok(Box::new(Steady { frames: 1152, channels }) as Box<dyn AudioDecode>));
        let output = Output { rate: 48_000, channels, bits: 16, codec_id: "mp3".into() };
        Bridge::new(&factory, output, origin, start).unwrap()
    }

    fn packet(seconds: f64, value: u8) -> Packet {
        let tb = oxideav_core::TimeBase::new(1, 48_000);
        let mut p = Packet::new(0, tb, vec![value]);
        p.pts = Some(tb.ticks_of(seconds));
        p
    }

    #[test]
    fn a_refused_packet_with_a_duration_stays_silent_and_keeps_the_rest_in_place() {
        // MP3-sized packets; the second is refused. Closing up behind it
        // would put everything after it 24 ms early.
        let mut b = bridge(1, 0.0, 0);
        let span = 1152.0 / 48_000.0;
        for (i, value) in [9u8, 0, 9, 9].into_iter().enumerate() {
            let mut p = packet(i as f64 * span, value);
            p.duration = Some(1152);
            b.feed(&p, None);
        }
        assert_eq!(b.decoded_to(), 4 * 1152);
        assert!(b.buf[1152..2 * 1152].iter().all(|&v| v == 0.0));
        assert!(b.buf[2 * 1152] > 0.0);
    }

    #[test]
    fn a_stamp_far_ahead_is_damage_not_twenty_minutes_of_silence() {
        let mut b = bridge(2, 0.0, 0);
        b.feed(&packet(0.0, 9), None);
        b.feed(&packet(1200.0, 9), None);
        b.feed(&packet(2.0 * 1152.0 / 48_000.0, 9), None);
        // The damaged packet's audio follows on where the stream was.
        assert_eq!(b.decoded_to(), 3 * 1152);
        assert_eq!(b.buf.len(), 3 * 1152 * 2);
    }

    #[test]
    fn a_refused_duration_cannot_carry_the_silence_past_the_cap() {
        // A refused packet claiming 100 s, then audio stamped where that
        // claim ends: the cap counts from the buffer's end, so at most
        // MAX_GAP_SECS of silence comes before the audio.
        let mut b = bridge(1, 0.0, 0);
        b.feed(&packet(0.0, 9), None);
        let mut p = packet(0.024, 0);
        p.duration = Some(100 * 48_000);
        b.feed(&p, None);
        b.feed(&packet(100.024, 9), None);
        let cap = (MAX_GAP_SECS * 48_000.0) as i64;
        assert_eq!(b.decoded_to(), 1152 + cap + 1152);
        assert!(b.buf[(1152 + cap) as usize] > 0.0);
    }

    #[test]
    fn refused_durations_neither_add_up_nor_overflow() {
        // Two refused packets of 20 min, one of i64::MAX ticks, then audio.
        let mut b = bridge(2, 0.0, 0);
        b.feed(&packet(0.0, 9), None);
        let mut at = 0.024;
        for duration in [1200 * 48_000, 1200 * 48_000, i64::MAX] {
            let mut p = packet(at, 0);
            p.duration = Some(duration);
            b.feed(&p, None);
            at += 1200.0;
        }
        b.feed(&packet(at, 9), None);
        let cap = (MAX_GAP_SECS * 48_000.0) as i64;
        assert_eq!(b.decoded_to(), 1152 + cap + 1152);
        assert_eq!(b.buf.len() as i64, b.decoded_to() * 2);
        assert!(b.buf[((1152 + cap) * 2) as usize] > 0.0);
    }

    #[test]
    fn a_restart_keeps_the_grid_a_sequential_run_has() {
        // Packets of 1152 samples (MP3) from 0 s; item 0 is source -0.084 s.
        let origin = -0.084;
        let packets: Vec<Packet> = (0..200).map(|i| packet(i as f64 * 1152.0 / 48_000.0, (i % 200) as u8 + 1)).collect();
        let mut sequential = bridge(2, origin, 0);
        for p in &packets {
            sequential.feed(p, None);
        }
        let all = sequential.blocks(40 * BLOCK);
        // A restart at the block holding 2 s: fed from a second earlier.
        let start = ((2.0 - origin) * 48_000.0 / BLOCK as f64).floor() as i64 * BLOCK;
        let mut restarted = bridge(2, origin, start);
        for p in packets.iter().filter(|p| p.time_base.seconds_of(p.pts.unwrap()) >= 1.0) {
            restarted.feed(p, None);
        }
        let again = restarted.blocks(40 * BLOCK);
        let first = all.iter().position(|b| b.at == start).expect("the grid holds the restart point");
        assert_eq!(again.len(), all.len() - first);
        for (a, b) in again.iter().zip(&all[first..]) {
            assert_eq!((a.at, a.samples, &a.data), (b.at, b.samples, &b.data));
        }
    }

    #[test]
    fn silence_fills_up_to_the_first_sample_and_over_gaps() {
        // Item 0 is source 0; the first packet comes 0.05 s in, the next
        // after a 0.2 s hole.
        let mut b = bridge(1, 0.0, 0);
        b.feed(&packet(0.05, 128), None);
        b.feed(&packet(0.05 + 1152.0 / 48_000.0 + 0.2, 128), None);
        assert_eq!(b.buf[..2400].iter().filter(|&&v| v != 0.0).count(), 0);
        assert!(b.buf[2400] > 0.0);
        let hole = 2400 + 1152;
        assert_eq!(b.buf[hole..hole + 9600].iter().filter(|&&v| v != 0.0).count(), 0);
        assert_eq!(b.decoded_to(), 2400 + 1152 + 9600 + 1152);
    }

    #[test]
    fn jitter_within_the_tolerance_stays_contiguous() {
        // Millisecond-rounded stamps (Matroska) wobble around the true start.
        let mut b = bridge(1, 0.0, 0);
        b.feed(&packet(0.0, 1), None);
        b.feed(&packet(0.024, 1), None); // true start 0.024 s, 1152 samples
        b.feed(&packet(0.047, 1), None); // true start 0.048 s
        assert_eq!(b.decoded_to(), 3 * 1152);
    }

    #[test]
    fn vorbis_order_becomes_flac_order() {
        // Vorbis 5.1: L C R Ls Rs LFE -> FLAC: L R C LFE Ls Rs.
        let map = channel_map("vorbis", 6).unwrap();
        let vorbis = ["L", "C", "R", "Ls", "Rs", "LFE"];
        let flac: Vec<&str> = map.iter().map(|&i| vorbis[i]).collect();
        assert_eq!(flac, ["L", "R", "C", "LFE", "Ls", "Rs"]);
        let vorbis = ["L", "C", "R", "Ls", "Rs", "Lb", "Rb", "LFE"];
        let flac: Vec<&str> = channel_map("vorbis", 8).unwrap().iter().map(|&i| vorbis[i]).collect();
        assert_eq!(flac, ["L", "R", "C", "LFE", "Lb", "Rb", "Ls", "Rs"]);
        assert!(channel_map("truehd", 8).is_none());
    }

    #[test]
    fn frames_decode_back_to_their_samples() {
        // Full and short blocks, 16 and 24 bits, rates with and without a
        // header code, frame numbers coded in 1 to 5 bytes (48 kHz blocks
        // 10.9 s, 3 min, 93 min and 70 h in), full scale both ways.
        for (rate, channels, bits) in [(48_000u32, 6usize, 24u16), (44_100, 2, 16), (37_800, 1, 16)] {
            let output = Output { rate, channels, bits, codec_id: "truehd".into() };
            let streaminfo = StreamInfo::parse(&streaminfo(&output)[4..]).unwrap();
            assert_eq!((streaminfo.sample_rate, streaminfo.channels, streaminfo.bits_per_sample), (rate, channels as u8, bits as u8));
            assert_eq!((streaminfo.min_block_size, streaminfo.max_block_size), (4096, 4096));
            let scale = f64::from(1u32 << (bits - 1));
            for (number, frames) in [(0u64, 4096usize), (200, 1000), (5_000, 4096), (70_000, 1), (3_000_000, 4096)] {
                let samples: Vec<f32> = (0..frames * channels)
                    .map(|i| match i % 7 {
                        0 => 1.0,
                        1 => -1.0,
                        2 => 1.5,
                        _ => ((i as f32) * 0.37).sin() * 0.8,
                    })
                    .collect();
                let data = frame(&output, number, &samples);
                assert_eq!(parse_frame_header(&data).unwrap().coded_number, number);
                // Checks both CRCs.
                let decoded = decode_frame_channels(&data, &streaminfo).unwrap();
                assert_eq!((decoded.channels.len(), decoded.frame_byte_len), (channels, data.len()));
                for (c, plane) in decoded.channels.iter().enumerate() {
                    assert_eq!(plane.len(), frames);
                    for (f, &got) in plane.iter().enumerate() {
                        let want = (f64::from(samples[f * channels + c]) * scale).round().clamp(-scale, scale - 1.0) as i32;
                        assert_eq!(got, want, "rate {rate} block {number} channel {c} sample {f}");
                    }
                }
            }
        }
    }
}
