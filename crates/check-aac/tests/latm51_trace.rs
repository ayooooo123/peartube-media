use oxideav_core::RuntimeContext;
use std::fs::File;

#[test]
fn latm51_layout_trace() {
    let path = refcheck::fate("aac/latm_stereo_to_51.ts");
    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);
    let mut demuxer = ctx.containers.open_demuxer("mpegts", Box::new(File::open(&path).unwrap()), &ctx.codecs).unwrap();
    let stream = demuxer.streams()[0].clone();
    let mut dec = ctx.codecs.first_decoder(&stream.params).unwrap();
    let mut histo: std::collections::BTreeMap<usize, usize> = Default::default();
    let mut first6 = None;
    let mut n = 0usize;
    loop {
        match demuxer.next_packet() {
            Ok(pkt) => {
                if pkt.stream_index != 0 { continue; }
                if dec.send_packet(&pkt).is_err() { println!("send err at frame {n}"); break; }
                while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                    let ch = a.data[0].len() / 4 / a.samples.max(1) as usize;
                    *histo.entry(ch).or_default() += 1;
                    if ch == 6 && first6.is_none() { first6 = Some(n); }
                    n += 1;
                }
            }
            Err(_) => break,
        }
    }
    println!("histo={histo:?} total={n} first6={first6:?}");
}
