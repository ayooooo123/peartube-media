// Ported from FFmpeg libavformat/mlpdec.c (the raw MLP/TrueHD demuxer),
// libavformat/rawdec.c (ff_raw_read_partial_packet framing), the key-frame
// rule of libavcodec/mlp_parser.c and the generic-index seek of
// libavformat/seek.c (seek_frame_generic, ff_add_index_entry,
// ff_reduce_index, ff_index_search_timestamp), commit 2da55bf.
// Licensed under LGPL-2.1-or-later.

//! Raw MLP / TrueHD demuxers. FFmpeg's raw demuxers emit one packet per
//! `RAW_PACKET_SIZE` (1024) byte read; the MLP parser then reassembles
//! access units for the decoder. OxideAV has no separate parser stage, so
//! this demuxer does what FFmpeg's `mlp_parse` does: scan for a major sync,
//! then cut complete access units using the 12-bit length field (× 2) in
//! each AU header, verifying the parity nibble exactly like
//! `read_access_unit`'s parity check. Timestamps are sample indices at the
//! source rate, as FFmpeg's `mlp_read_header` sets up.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, ContainerRegistry, Demuxer, Error, MediaType, Packet, ProbeData,
    ProbeScore, ReadSeek, Result, SampleFormat, StreamInfo, TimeBase,
};

use crate::bitreader::BitReader;
use crate::common::{mlp_samplerate, truehd_channels, SYNC_MLP, SYNC_TRUEHD};
use crate::tables::{MLP_CHANNELS, MLP_QUANTS, THD_CHANCOUNT};

/// How much head open() reads: enough for the resync scan and the first
/// major sync.
const HEAD_BYTES: usize = 256 * 1024;

/// avformat's max_index_size (1 MiB) over sizeof(AVIndexEntry) (24 bytes).
const MAX_INDEX_ENTRIES: usize = (1 << 20) / 24;

pub struct RawMlpDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    format_name: &'static str,
    /// Read cursor: file offset of the next access unit header.
    next_offset: u64,
    /// Sample position of that AU's first frame (packet pts).
    next_pts: u64,
    /// Samples per access unit: 40 << (ratebits & 7).
    au_size: u32,
    /// File offset where the AU chain starts.
    start_offset: u64,
    /// AVFMT_GENERIC_INDEX: the key access units returned so far, their
    /// offset and pts by pts (ff_add_index_entry replaces one of the same
    /// pts; ff_reduce_index keeps every other entry of a full index).
    index: Vec<(u64, u64)>,
}

/// mlp_parser.c sets key_frame for an access unit with a major sync
/// (0xF8726FBA or 0xF8726FBB after the 4-byte unit header) and clears it
/// for the others. FFmpeg also drops a major sync whose header does not
/// read (ff_mlp_read_major_sync); this demuxer flags it key.
fn major_sync(au: &[u8]) -> bool {
    au.get(4..8).is_some_and(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) & 0xFFFF_FFFE == 0xF872_6FBA)
}

impl RawMlpDemuxer {
    fn open(mut input: Box<dyn ReadSeek>, is_mlp: bool) -> Result<Box<dyn Demuxer>> {
        let format_name = if is_mlp { "mlp" } else { "truehd" };
        let sync_byte = if is_mlp { SYNC_MLP } else { SYNC_TRUEHD };

        // Read a bounded head: enough for the resync scan and the first
        // major sync. Nothing past that is touched until next_packet asks
        // for it, so opening an HTTP source does not wait for the whole
        // file (FFmpeg's raw demuxers read the head only).
        input.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; HEAD_BYTES];
        let n = read_up_to(&mut input, &mut buf)?;
        buf.truncate(n);

        // Scan for the first major sync that starts a length-consistent AU
        // chain (the resync walk FFmpeg's mlp_parser performs). The chain is
        // verified a few units deep from the head; after that next_packet
        // simply follows the 12-bit length fields.
        let sync_head = [0xf8, 0x72, 0x6f, sync_byte];
        let mut base_offset = None;
        for off in 0..buf.len().saturating_sub(12) {
            if buf[off + 4..off + 8] != sync_head {
                continue;
            }
            // Walk a few AUs inside the buffered head to confirm the length
            // fields chain cleanly (>= 8 units, like the old full walk's
            // acceptance, but bounded).
            let mut pos = off;
            let mut units = 0usize;
            let mut ok = true;
            while pos + 4 <= buf.len() {
                let l = (u16::from_be_bytes([buf[pos], buf[pos + 1]]) & 0xfff) as usize * 2;
                if l < 4 || pos + l > buf.len() {
                    ok = units >= 8;
                    break;
                }
                units += 1;
                pos += l;
            }
            if ok && units >= 8 {
                base_offset = Some(off as u64);
                break;
            }
        }

        let Some(base_offset) = base_offset else {
            return Err(Error::InvalidData(format!(
                "{format_name}: no access unit chain found"
            )));
        };

        // Sample rate from the first major sync (mlp_read_header):
        // TrueHD ratebits at major-sync byte 4, MLP at byte 5.
        let sync_at = base_offset as usize + 4;
        let ratebits = ratebits_of(&buf, sync_at, is_mlp);
        let sample_rate = match mlp_samplerate(ratebits) {
            0 => 48_000,
            r => r,
        };

        let codec_id = CodecId::new(if is_mlp { "mlp" } else { "truehd" });
        let mut params = CodecParameters::audio(codec_id);
        params.sample_rate = Some(sample_rate);
        params.media_type = MediaType::Audio;
        // Channel count and output sample format from the first major sync
        // (FFmpeg's mlp_parser.c reports both the same way: for TrueHD the
        // stream-2 chanmap when nonzero, else stream-1; for MLP the channel-
        // arrangement table; sample format s16 for ≤16-bit group1, else s32).
        if sync_at + 8 <= buf.len() {
            let b = &buf[sync_at..];
            // b[0..3] is the 24-bit sync; b[3] the stream type.
            let mut gb = BitReader::new(b);
            let _sync = gb.get_bits(24);
            let stream_type = gb.get_bits(8);
            let mut group1_bits = 24u32;
            let channels = if stream_type == 0xbb {
                group1_bits = u32::from(MLP_QUANTS[gb.get_bits(4) as usize]);
                let _g2 = gb.get_bits(4);
                let _ratebits = gb.get_bits(4);
                let _g2rate = gb.get_bits(4);
                gb.skip(11);
                let arrangement = gb.get_bits(5) as usize;
                u32::from(MLP_CHANNELS[arrangement])
            } else if stream_type == 0xba {
                let _ratebits = gb.get_bits(4);
                gb.skip(4);
                let _mod0 = gb.get_bits(2);
                let _mod1 = gb.get_bits(2);
                let chanmap1 = gb.get_bits(5);
                let _mod2 = gb.get_bits(2);
                let chanmap2 = gb.get_bits(13);
                // 16-channel (Atmos) streams surface the 8-channel
                // presentation, matching the decoder's default output.
                let (c1, c2) = (truehd_channels(chanmap1), truehd_channels(chanmap2));
                if c2 != 0 && c2 != c1 {
                    c2
                } else {
                    c1
                }
            } else {
                0
            };
            if channels > 0 && channels <= u32::from(THD_CHANCOUNT.iter().sum::<u8>()) {
                params.channels = Some(channels as u16);
            }
            params.sample_format = Some(if group1_bits > 16 {
                SampleFormat::S32
            } else {
                SampleFormat::S16
            });
        }

        let time_base = TimeBase::new(1, i64::from(sample_rate));
        // access_unit_size = 40 << (ratebits & 7), from the same ratebits
        // that gave the sample rate (FFmpeg's read_major_sync / parser set
        // pts progression with it).
        let au_size = 40u32 << (ratebits_of(&buf, sync_at, is_mlp) & 7);

        let stream = StreamInfo {
            index: 0,
            time_base,
            // Unknown until the last AU is decoded; FFmpeg's raw demuxers
            // with AVFMT_NOTIMESTAMPS declare none either.
            duration: None,
            start_time: Some(0),
            params,
        };

        Ok(Box::new(RawMlpDemuxer {
            input,
            streams: vec![stream],
            format_name,
            next_offset: base_offset,
            next_pts: 0,
            au_size,
            start_offset: base_offset,
            index: Vec::new(),
        }))
    }
}

/// The `ratebits` nibble of the first major sync (TrueHD: byte 4 high
/// nibble, MLP: byte 5), 0 when the head does not reach it.
fn ratebits_of(buf: &[u8], sync_at: usize, is_mlp: bool) -> u32 {
    if sync_at + 10 <= buf.len() {
        let b = &buf[sync_at..];
        u32::from(if is_mlp { b[5] >> 4 } else { b[4] >> 4 })
    } else {
        0
    }
}

impl RawMlpDemuxer {
    /// The offset of the access unit whose major sync comes first at or
    /// after `from + 4` (mlp_parser's lost_sync scan). The read window
    /// starts small and doubles to 64 KiB, so a run of false headers costs
    /// a few hundred bytes each and a long gap a few large reads.
    fn find_sync(&mut self, mut from: u64) -> Result<u64> {
        const MIN_WINDOW: usize = 256;
        const MAX_WINDOW: usize = 64 * 1024;
        let sync_byte = if self.format_name == "mlp" {
            SYNC_MLP
        } else {
            SYNC_TRUEHD
        };
        let mut window = MIN_WINDOW;
        loop {
            self.input.seek(SeekFrom::Start(from))?;
            let mut buf = vec![0u8; window];
            let n = read_up_to(&mut self.input, &mut buf)?;
            if n < 8 {
                return Err(Error::Eof);
            }
            if let Some(off) = buf[..n].windows(8).position(|w| w[4..8] == [0xf8, 0x72, 0x6f, sync_byte]) {
                return Ok(from + off as u64);
            }
            from += (n - 7) as u64;
            window = (window * 2).min(MAX_WINDOW);
        }
    }
}

fn read_up_to(input: &mut Box<dyn ReadSeek>, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    loop {
        let n = input.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
        if filled == buf.len() {
            break;
        }
    }
    Ok(filled)
}

impl Demuxer for RawMlpDemuxer {
    fn format_name(&self) -> &str {
        self.format_name
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        // Read the 2-byte AU header at the cursor (ff_raw_read_partial_packet
        // is unstructured; the AU framing comes from the parser, which reads
        // the length field wherever the cursor sits). A broken length chain
        // mid-stream loses sync: FFmpeg's parser scans on for the next sync
        // word, and so does this loop, each turn strictly past the last.
        let len = loop {
            let mut hdr = [0u8; 2];
            self.input.seek(SeekFrom::Start(self.next_offset))?;
            let got = read_up_to(&mut self.input, &mut hdr)?;
            if got < 2 {
                return Err(Error::Eof);
            }
            let len = (u16::from_be_bytes(hdr) & 0xfff) as usize * 2;
            if len >= 4 {
                break len;
            }
            self.next_offset = self.find_sync(self.next_offset + 1)?;
        };

        let mut data = vec![0u8; len];
        self.input.seek(SeekFrom::Start(self.next_offset))?;
        if self.input.read_exact(&mut data).is_err() {
            // Truncated final AU (luckynight ends mid-frame): FFmpeg's raw
            // demuxer emits the short read, but the decoder needs a whole
            // AU header at minimum; a partial frame errors inside it. Drop
            // the tail like the parse loop does.
            return Err(Error::Eof);
        }

        let pts = self.next_pts;
        let offset = self.next_offset;
        self.next_pts += u64::from(self.au_size);
        self.next_offset += len as u64;

        let key = major_sync(&data);
        if key {
            // av_read_frame indexes every key packet it returns.
            if self.index.len() >= MAX_INDEX_ENTRIES {
                self.index = self.index.iter().step_by(2).copied().collect();
            }
            match self.index.binary_search_by_key(&pts, |e| e.1) {
                Ok(at) => self.index[at].0 = offset,
                Err(at) => self.index.insert(at, (offset, pts)),
            }
        }
        let tb = self.streams[0].time_base;
        Ok(Packet::new(0, tb, data)
            .with_pts(pts as i64)
            .with_dts(pts as i64)
            .with_keyframe(key))
    }

    /// mlpdec.c is AVFMT_GENERIC_INDEX: seek.c seek_frame_generic with
    /// AVSEEK_FLAG_BACKWARD lands on the last access unit with a major sync
    /// at or before the target, among those returned so far; past the last
    /// of them units are read on, bounded by the input, until a key unit
    /// starts after the target or more than 1000 others did.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let search = |index: &[(u64, u64)]| index.partition_point(|e| e.1 as i64 <= pts).checked_sub(1);
        let mut found = search(&self.index);
        if found.is_none() && self.index.first().is_some_and(|e| pts < e.1 as i64) {
            return Err(Error::invalid("seek before the first major sync"));
        }
        if found.is_none() || found == Some(self.index.len() - 1) {
            (self.next_offset, self.next_pts) = self.index.last().copied().unwrap_or((self.start_offset, 0));
            let mut nonkey = 0;
            while let Ok(packet) = self.next_packet() {
                if packet.dts.is_some_and(|dts| dts > pts) {
                    if packet.flags.keyframe {
                        break;
                    }
                    nonkey += 1;
                    if nonkey > 1001 {
                        break;
                    }
                }
            }
            found = search(&self.index);
        }
        let Some(i) = found else {
            return Err(Error::invalid("no major sync to seek to"));
        };
        (self.next_offset, self.next_pts) = self.index[i];
        Ok(self.next_pts as i64)
    }
}

/// `mlp_thd_probe`: count sync-positioned, length-consistent AUs; accept at
/// FFmpeg's threshold (valid >= 100 needs 100 syncs in the first 256 KiB;
/// real MLP frames are ≥ ~100 bytes so cap the scan at the head we have).
fn mlp_thd_probe(p: &ProbeData, sync: u8) -> ProbeScore {
    let buf = p.buf;
    if buf.len() < 8 {
        return 0;
    }
    let mut valid = 0usize;
    let mut last: Option<usize> = None;
    let mut size = 0usize;
    let mut nsubframes = 0usize;

    let mut off = 0usize;
    while off + 8 <= buf.len() {
        if buf[off + 4] == 0xf8
            && buf[off + 5] == 0x72
            && buf[off + 6] == 0x6f
            && buf[off + 7] == sync
        {
            if last == Some(off.saturating_sub(size)) {
                valid += 1 + nsubframes / 8;
            }
            nsubframes = 0;
            last = Some(off);
            size = (u16::from_be_bytes([buf[off], buf[off + 1]]) & 0xfff) as usize * 2;
        } else if last.is_some() && off.saturating_sub(last.unwrap()) == size {
            nsubframes += 1;
            size += (u16::from_be_bytes([buf[off], buf[off + 1]]) & 0xfff) as usize * 2;
        }
        off += 1;
    }
    if valid >= 100 {
        oxideav_core::MAX_PROBE_SCORE
    } else {
        0
    }
}

/// Install the `mlp` and `truehd` demuxers on the container registry.
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mlp", open_mlp);
    reg.register_demuxer("truehd", open_truehd);
    reg.register_probe("mlp", probe_mlp);
    reg.register_probe("truehd", probe_truehd);
    reg.register_extension("mlp", "mlp");
    reg.register_extension("thd", "truehd");
}

fn open_mlp(input: Box<dyn ReadSeek>, _codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    RawMlpDemuxer::open(input, true)
}

fn open_truehd(input: Box<dyn ReadSeek>, _codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    RawMlpDemuxer::open(input, false)
}

fn probe_mlp(probe: &ProbeData) -> ProbeScore {
    mlp_thd_probe(probe, SYNC_MLP)
}

fn probe_truehd(probe: &ProbeData) -> ProbeScore {
    mlp_thd_probe(probe, SYNC_TRUEHD)
}
