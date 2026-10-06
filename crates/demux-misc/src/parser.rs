// Ported from FFmpeg (commit 2da55bf): libavcodec/parser.c
// (av_parser_parse2, ff_fetch_timestamp) and libavcodec/dvdsub_parser.c,
// with the parse_packet loop of libavformat/demux.c that drives them.
// License: LGPL-2.1-or-later
//
// FFmpeg's parser stage, for demuxers whose packets are not the units a
// decoder takes. A `Parser` is fed one stream's demuxed packets in order
// and returns whole units, each carrying the timestamps FFmpeg's parser
// stage hands the unit's packet.

/// One codec's unit splitter: the `parse` callback of an FFmpeg parser.
/// It consumes a prefix of `buf` (all of it unless a unit ends inside)
/// and returns a completed unit; an empty `buf` is the end of input.
pub(crate) trait Split {
    fn parse(&mut self, buf: &[u8]) -> (usize, Option<Vec<u8>>);
}

/// A parsed unit and the timestamps of the demuxed packet it inherits.
pub(crate) struct Unit {
    pub data: Vec<u8>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
}

/// AV_PARSER_PTS_NB: demuxed packets whose timestamps are remembered.
const PTS_NB: usize = 4;

/// The timestamp bookkeeping of AVCodecParserContext: which demuxed
/// packet's timestamps a unit inherits. Offsets count input bytes from the
/// first packet's position on.
struct Timestamps {
    fetched_offset: bool,
    fetch_timestamp: bool,
    cur_offset: i64,
    frame_offset: i64,
    next_frame_offset: i64,
    start_index: usize,
    offset: [i64; PTS_NB],
    end: [i64; PTS_NB],
    pts: [Option<i64>; PTS_NB],
    dts: [Option<i64>; PTS_NB],
    out_pts: Option<i64>,
    out_dts: Option<i64>,
}

impl Default for Timestamps {
    fn default() -> Self {
        Self {
            fetched_offset: false,
            // av_parser_init sets fetch_timestamp.
            fetch_timestamp: true,
            cur_offset: 0,
            frame_offset: 0,
            next_frame_offset: 0,
            start_index: 0,
            offset: [0; PTS_NB],
            end: [0; PTS_NB],
            pts: [None; PTS_NB],
            dts: [None; PTS_NB],
            out_pts: None,
            out_dts: None,
        }
    }
}

impl Timestamps {
    /// ff_fetch_timestamp(s, 0, 0, 0): the timestamps of the last packet
    /// that starts at or before the current offset and after the last
    /// unit's start.
    fn fetch(&mut self) {
        self.out_pts = None;
        self.out_dts = None;
        for i in 0..PTS_NB {
            if self.cur_offset >= self.offset[i]
                && (self.frame_offset < self.offset[i] || (self.frame_offset == 0 && self.next_frame_offset == 0))
                && self.end[i] != 0
            {
                self.out_dts = self.dts[i];
                self.out_pts = self.pts[i];
                if self.cur_offset < self.end[i] {
                    break;
                }
            }
        }
    }
}

/// An FFmpeg parser: a [`Split`] plus the timestamp bookkeeping of
/// av_parser_parse2.
pub(crate) struct Parser<S> {
    split: S,
    ts: Timestamps,
}

impl<S: Split> Parser<S> {
    pub fn new(split: S) -> Self {
        Self { split, ts: Timestamps::default() }
    }

    /// av_parser_parse2: one call of the splitter on `buf`, which starts
    /// a demuxed packet when `pos` is `Some`. Returns the bytes consumed.
    fn parse2(&mut self, buf: &[u8], pts: Option<i64>, dts: Option<i64>, pos: Option<i64>, out: &mut Vec<Unit>) -> usize {
        let ts = &mut self.ts;
        if !ts.fetched_offset {
            ts.cur_offset = pos.unwrap_or(-1);
            ts.next_frame_offset = ts.cur_offset;
            ts.fetched_offset = true;
        }
        let len = buf.len() as i64;
        if !buf.is_empty() && ts.cur_offset + len != ts.end[ts.start_index] {
            // A new packet: remember where it lies and its timestamps.
            let i = (ts.start_index + 1) & (PTS_NB - 1);
            ts.start_index = i;
            ts.offset[i] = ts.cur_offset;
            ts.end[i] = ts.cur_offset + len;
            ts.pts[i] = pts;
            ts.dts[i] = dts;
        }
        if ts.fetch_timestamp {
            ts.fetch_timestamp = false;
            ts.fetch();
        }
        let (index, unit) = self.split.parse(buf);
        let ts = &mut self.ts;
        if let Some(data) = unit {
            ts.frame_offset = ts.next_frame_offset;
            ts.next_frame_offset = ts.cur_offset + index as i64;
            ts.fetch_timestamp = true;
            out.push(Unit { data, pts: ts.out_pts, dts: ts.out_dts });
        }
        ts.cur_offset += index as i64;
        index
    }

    /// parse_packet: feed one demuxed packet, collecting completed units.
    pub fn push(&mut self, data: &[u8], pts: Option<i64>, dts: Option<i64>, pos: i64, out: &mut Vec<Unit>) {
        let mut rest = data;
        let (mut pts, mut dts, mut pos) = (pts, dts, Some(pos));
        while !rest.is_empty() {
            let used = self.parse2(rest, pts, dts, pos, out).min(rest.len());
            (pts, dts, pos) = (None, None, None);
            rest = &rest[used..];
        }
    }

    /// parse_packet with flush at the end of input: drain what the
    /// splitter still holds.
    pub fn flush(&mut self, out: &mut Vec<Unit>) {
        loop {
            let before = out.len();
            self.parse2(&[], None, None, None, out);
            if out.len() == before {
                break;
            }
        }
    }
}

/// The largest DVD subpicture unit assembled; FFmpeg allows up to
/// INT_MAX bytes. A unit whose header claims more is dropped.
pub(crate) const MAX_SPU_SIZE: usize = 1 << 20;

/// dvdsub_parser.c: reassembles a DVD subpicture unit from the PES
/// payloads it spans. Its first two bytes give its size, or, when 0
/// (HD-DVD), the four bytes after them.
#[derive(Default)]
pub(crate) struct DvdSub {
    packet: Vec<u8>,
    packet_len: usize,
    packet_index: usize,
    allocated: bool,
}

impl Split for DvdSub {
    fn parse(&mut self, buf: &[u8]) -> (usize, Option<Vec<u8>>) {
        let n = buf.len();
        if self.packet_index == 0 {
            let be16 = |b: &[u8]| usize::from(u16::from_be_bytes([b[0], b[1]]));
            if n < 2 || (be16(buf) == 0 && n < 6) {
                // Too small to start a unit: passed on as it is.
                return (n, (n > 0).then(|| buf.to_vec()));
            }
            let mut len = be16(buf);
            if len == 0 {
                len = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize;
            }
            self.packet.clear();
            self.allocated = len <= MAX_SPU_SIZE;
            self.packet_len = len;
        }
        if self.allocated {
            if n <= self.packet_len - self.packet_index {
                self.packet.extend_from_slice(buf);
                self.packet_index += n;
                if self.packet_index >= self.packet_len {
                    self.packet_index = 0;
                    return (n, Some(std::mem::take(&mut self.packet)));
                }
            } else {
                // Erroneous size: the payload overruns the unit.
                self.packet_index = 0;
            }
        }
        (n, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(parser: &mut Parser<DvdSub>, packets: &[(&[u8], Option<i64>)]) -> Vec<(Vec<u8>, Option<i64>)> {
        let mut out = Vec::new();
        for (i, (data, pts)) in packets.iter().enumerate() {
            parser.push(data, *pts, *pts, 1000 + 2048 * i as i64, &mut out);
        }
        parser.flush(&mut out);
        out.into_iter().map(|u| (u.data, u.pts)).collect()
    }

    #[test]
    fn spu_spanning_packets_is_one_unit_with_the_first_packets_pts() {
        let spu: Vec<u8> = [0x00, 0x0A].iter().copied().chain(2..10).collect();
        let mut parser = Parser::new(DvdSub::default());
        let got = units(&mut parser, &[(&spu[..4], Some(900)), (&spu[4..7], None), (&spu[7..], None)]);
        assert_eq!(got, vec![(spu, Some(900))]);
    }

    #[test]
    fn payload_overrunning_its_spu_is_dropped_and_the_next_spu_starts_clean() {
        let mut parser = Parser::new(DvdSub::default());
        let overrun = [0x00, 0x04, 1, 2, 3];
        let next = [0x00, 0x03, 7];
        let got = units(&mut parser, &[(&overrun[..], Some(10)), (&next[..], Some(20))]);
        // av_parser_parse2 fetches timestamps again only after it output
        // a unit, so the unit after a dropped one keeps the stale fetch.
        assert_eq!(got, vec![(next.to_vec(), Some(10))]);
    }

    #[test]
    fn oversized_spu_is_not_assembled() {
        let mut parser = Parser::new(DvdSub::default());
        let header = [0x00, 0x00, 0x7F, 0xFF, 0xFF, 0xFF, 1, 2];
        let got = units(&mut parser, &[(&header[..], Some(10))]);
        assert!(got.is_empty());
    }
}
