//! Small ALS streams exercising the send/receive memory bound. The old
//! eager path expands at most 2 MiB here, never the review's 16 GiB case.
use super::*;
use oxideav_core::TimeBase;

fn params(samples: u32) -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new("mp4als"));
    // AOT 36, 48 kHz, mono, followed by ALSSpecificConfig: S32,
    // 65536 samples/frame, no prediction, no random-access size words.
    p.extradata = vec![0xF8, 0x86, 0x20];
    p.extradata.extend_from_slice(b"ALS\0");
    p.extradata.extend_from_slice(&48000u32.to_be_bytes());
    p.extradata.extend_from_slice(&samples.to_be_bytes());
    p.extradata.extend_from_slice(&[0, 0, 12, 255, 255, 0]);
    p.extradata.extend_from_slice(&[0; 12]);
    p
}

fn packet(data: Vec<u8>, pts: i64) -> Packet {
    let mut p = Packet::new(0, TimeBase::new(1, 48000), data);
    p.pts = Some(pts);
    p
}

fn frame(d: &mut AlsDecoder, samples: u32, pts: Option<i64>, value: i32) {
    let before = d.frame_id;
    let Frame::Audio(a) = d.receive_frame().expect("next frame") else { panic!("audio") };
    assert!(d.frame_id <= before + 1, "a receive decoded ahead of its output");
    assert_eq!((a.samples, a.pts), (samples, pts));
    assert_eq!(a.data.len(), 1);
    assert_eq!(a.data[0].len(), samples as usize * 4);
    assert!(a.data[0].chunks_exact(4).all(|s| s == value.to_le_bytes()));
}

#[test]
fn bounds_als_packet_drains_one_frame_at_a_time() {
    let mut d = AlsDecoder::new(&params(8 * 65536 - 11)).unwrap();
    d.send_packet(&packet(vec![0; 8], 19)).unwrap();
    assert!(d.frame_id <= 1, "send decoded {} frames before any receive", d.frame_id);
    d.flush().unwrap();
    assert!(d.frame_id <= 1, "flush eagerly expanded the packet");
    for i in 0..8 {
        frame(&mut d, if i == 7 { 65536 - 11 } else { 65536 }, (i == 0).then_some(19), 0);
    }
    assert!(matches!(d.receive_frame(), Err(Error::NeedMore | Error::Eof)));
    d.flush().unwrap();
    assert!(matches!(d.receive_frame(), Err(Error::NeedMore | Error::Eof)));
}

#[test]
fn bounds_als_packet_order_timestamps_and_reset() {
    let mut d = AlsDecoder::new(&params(u32::MAX)).unwrap();
    let mut constant = vec![0x40];
    constant.extend_from_slice(&12345i32.to_be_bytes());
    d.send_packet(&packet(vec![0, 0], 7)).unwrap();
    d.send_packet(&packet(constant.clone(), 99)).unwrap();
    frame(&mut d, 65536, Some(7), 0);
    frame(&mut d, 65536, None, 0);
    frame(&mut d, 65536, Some(99), 12345);
    assert!(matches!(d.receive_frame(), Err(Error::NeedMore | Error::Eof)));
    d.send_packet(&packet(vec![0; 3], 0)).unwrap();
    frame(&mut d, 65536, Some(0), 0);
    d.reset().unwrap();
    assert!(matches!(d.receive_frame(), Err(Error::NeedMore | Error::Eof)));
    d.send_packet(&packet(constant, 42)).unwrap();
    frame(&mut d, 65536, Some(42), 12345);
    assert_eq!(d.frame_id, 1);
}
