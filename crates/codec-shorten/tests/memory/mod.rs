//! Output-progress checks, not huge allocations: the eager decoder retains
//! less than 300 KiB in either regression. Codes deliberately cross bytes.
use super::*;
use oxideav_core::TimeBase;

struct Writer(Vec<u8>, usize);
impl Writer {
    fn put(&mut self, n: u32, value: u32) {
        for i in (0..n).rev() {
            if self.1 % 8 == 0 { self.0.push(0); }
            let at = self.1 / 8;
            self.0[at] |= ((value >> i & 1) as u8) << (7 - self.1 % 8);
            self.1 += 1;
        }
    }
    fn ur(&mut self, k: u32, value: u32) {
        for _ in 0..value >> k { self.put(1, 0); }
        self.put(1, 1);
        self.put(k, value);
    }
    fn uint(&mut self, value: u32) {
        let k = 32 - value.leading_zeros();
        self.ur(2, k);
        self.ur(k, value);
    }
}

fn stream(blocksize: u32, blocks: usize) -> Vec<u8> {
    let mut w = Writer(Vec::new(), 0);
    w.put(32, u32::from_be_bytes(*b"ajkg"));
    w.put(8, 2);
    for v in [5, 2, blocksize, 0, 0, 0] { w.uint(v); }
    // The container supplies the WAVE header; only the Shorten header
    // and the two channel commands per block are in these packets.
    for _ in 0..blocks * 2 { w.ur(2, FN_ZERO); }
    w.ur(2, FN_QUIT);
    w.put(8, 0);
    w.0
}

fn decoder() -> ShortenDecoder {
    let mut p = CodecParameters::audio(CodecId::new("shorten"));
    p.extradata = vec![1];
    p.channels = Some(2);
    p.sample_rate = Some(8000);
    p.sample_format = Some(SampleFormat::S16P);
    ShortenDecoder::new(&p).unwrap()
}

fn drain(d: &mut ShortenDecoder, blocksize: u32, count: &mut usize) {
    loop {
        let before = d.state.next_pts;
        match d.receive_frame() {
            Ok(Frame::Audio(a)) => {
                assert!(d.state.next_pts <= before + i64::from(blocksize), "receive decoded ahead");
                assert_eq!((a.samples, a.pts), (blocksize, Some(*count as i64 * i64::from(blocksize))));
                assert_eq!(a.data.len(), 2);
                for p in &a.data {
                    assert_eq!(p.len(), blocksize as usize * 2);
                    assert!(p.iter().all(|&b| b == 0));
                }
                *count += 1;
            }
            Err(Error::NeedMore | Error::Eof) => break,
            other => panic!("unexpected receive: {other:?}"),
        }
    }
}

#[test]
fn bounds_shorten_flush_is_incremental_and_idempotent() {
    for chunk in [1, 7, 1024] {
        let mut d = decoder();
        let mut count = 0;
        for bytes in stream(4096, 17).chunks(chunk) {
            d.send_packet(&Packet::new(0, TimeBase::new(1, 8000), bytes.to_vec())).unwrap();
            drain(&mut d, 4096, &mut count);
        }
        let before = d.state.next_pts;
        d.flush().unwrap();
        assert!(d.state.next_pts <= before + 4096, "flush decoded {} blocks before receive", (d.state.next_pts - before) / 4096);
        drain(&mut d, 4096, &mut count);
        assert_eq!(count, 17);
        d.flush().unwrap();
        drain(&mut d, 4096, &mut count);
        assert_eq!(count, 17);
        d.reset().unwrap();
        d.send_packet(&Packet::new(0, TimeBase::new(1, 8000), stream(4096, 2))).unwrap();
        d.flush().unwrap();
        count = 0;
        drain(&mut d, 4096, &mut count);
        assert_eq!(count, 2);
    }
}

#[test]
fn bounds_shorten_large_packet_does_not_expand_on_send() {
    let mut d = decoder();
    d.send_packet(&Packet::new(0, TimeBase::new(1, 8000), stream(8, 7000))).unwrap();
    assert!(d.state.next_pts <= 8, "send decoded {} blocks before receive", d.state.next_pts / 8);
    // Flush before receiving must still consume every pending input byte.
    d.flush().unwrap();
    let mut count = 0;
    drain(&mut d, 8, &mut count);
    assert_eq!(count, 7000);
}
