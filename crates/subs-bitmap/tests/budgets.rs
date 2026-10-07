//! Tiny hostile packets must not buy huge canvases or unbounded paint
//! work: every frame stays within the 4096x4096 RGBA canvas cap, and each
//! sequence decodes within a time budget far above what valid streams of
//! the same size need.

use std::sync::mpsc;
use std::time::Duration;

use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, RuntimeContext, TimeBase};

/// Largest canvas a decoder may emit: 4096x4096 RGBA.
const CANVAS_CAP: usize = 4096 * 4096 * 4;

fn decoder(id: &str) -> Box<dyn Decoder> {
    let mut ctx = RuntimeContext::new();
    subs_bitmap::register(&mut ctx);
    ctx.codecs.first_decoder(&CodecParameters::subtitle(CodecId::new(id))).unwrap()
}

fn packet(pts: i64, data: Vec<u8>) -> Packet {
    let mut packet = Packet::new(0, TimeBase::new(1, 90_000), data);
    packet.pts = Some(pts);
    packet
}

/// Sends every packet, drains every frame, and returns the frame count;
/// fails on any frame beyond the canvas cap.
fn decode_all(decoder: &mut dyn Decoder, packets: impl IntoIterator<Item = Packet>) -> usize {
    let mut frames = 0;
    for packet in packets {
        let _ = decoder.send_packet(&packet);
        while let Ok(frame) = decoder.receive_frame() {
            let Frame::Video(frame) = frame else { panic!("not a canvas") };
            let bytes = frame.planes[0].data.len();
            assert!(bytes <= CANVAS_CAP, "a {bytes}-byte canvas from a {}-byte packet", packet.data.len());
            frames += 1;
        }
    }
    frames
}

/// Runs `work` on its own thread and fails unless it finishes within
/// `budget` (a runaway decode keeps its thread busy, not the test).
fn within<T: Send + 'static>(budget: Duration, what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    match finished.recv_timeout(budget) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what}: not within {budget:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what}: the decode panicked"),
    }
}

/// One DVB subtitling segment of page 1.
fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0x0f, kind, 0, 1];
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn dvb(segments: &[Vec<u8>]) -> Vec<u8> {
    let mut out = segments.concat();
    out.push(0xff);
    out
}

/// A display definition of 8192x8192, then a thousand 7-byte
/// end-of-display packets, each of which makes the decoder emit its
/// whole page.
#[test]
fn dvb_display_definition_cannot_buy_oversized_canvases() {
    let frames = within(Duration::from_secs(20), "8192x8192 display, 1000 end-of-display packets", || {
        let mut decoder = decoder("dvb_subtitle");
        let definition = dvb(&[segment(0x14, &[0x00, 0x1f, 0xff, 0x1f, 0xff])]);
        let end = dvb(&[segment(0x80, &[])]);
        assert_eq!(end.len(), 7);
        let packets = std::iter::once(packet(0, definition)).chain((1..=1000).map(move |i| packet(i * 3600, end.clone())));
        decode_all(&mut *decoder, packets)
    });
    assert_eq!(frames, 1000, "every end of display still yields its (blank) state");
}

/// A 30-byte OGT image header declaring 16383x4096 with no image data.
#[test]
fn ogt_header_cannot_buy_a_huge_region() {
    let mut spu = vec![0, 0, 0, 0];
    spu.extend_from_slice(&[0, 0, 0, 0, 0x3f, 0xff, 0x10, 0x00]);
    for _ in 0..4 {
        spu.extend_from_slice(&[0x80, 0x80, 0x80, 0xff]);
    }
    spu.extend_from_slice(&[0, 0, 0]);
    let mut data = vec![0x70, 0, 0x80, 0, 0];
    data.extend_from_slice(&spu);
    assert!(data.len() <= 40);
    within(Duration::from_secs(10), "100 OGT headers of 16383x4096", move || {
        let mut decoder = decoder("ogt");
        decode_all(&mut *decoder, (0..100).map(|i| packet(i * 9000, data.clone())))
    });
}

/// 1280x1024 regions (FFmpeg's largest) that each place one object
/// hundreds of times, then 64 KiB of that object's pixel data: without
/// bounds every placement repaints a whole region.
#[test]
fn dvb_object_fanout_and_paint_work_are_bounded() {
    let mut packets = Vec::new();
    for region in 0..=255u8 {
        let mut body = vec![region, 0x08, 0x05, 0x00, 0x04, 0x00, 0x08, 0, 0, 0];
        for _ in 0..320 {
            // Object 1 at (0, 0): every placement paints whole rows.
            body.extend_from_slice(&[0, 1, 0, 0, 0, 0]);
        }
        packets.push(dvb(&[segment(0x11, &body)]));
    }
    // Rows of 1280 4-bit pixels of colour 1: a full row reads the 8-bit
    // end code after it, then the end of line moves to the next row pair.
    let mut field = Vec::new();
    while field.len() < 32 * 1024 - 700 {
        field.push(0x11);
        field.extend(std::iter::repeat_n(0x11, 640));
        field.extend_from_slice(&[0x00, 0xf0]);
    }
    let mut object = vec![0, 1, 0];
    object.extend_from_slice(&(field.len() as u16).to_be_bytes());
    object.extend_from_slice(&(field.len() as u16).to_be_bytes());
    object.extend_from_slice(&field);
    object.extend_from_slice(&field);
    packets.push(dvb(&[segment(0x13, &object)]));
    within(Duration::from_secs(20), "256 regions x 320 placements, then 64 KiB of object data", move || {
        let mut decoder = decoder("dvb_subtitle");
        decode_all(&mut *decoder, packets.into_iter().enumerate().map(|(i, data)| packet(i as i64, data)))
    });
}

/// Region compositions alone, without any object, pay for the pixels they
/// write: a 16 KiB packet of 1,024 region segments filling one 1024x1024
/// region, or resizing it in every segment, would write 1 GiB. Each stops
/// with an error once the packet's paint work is spent, and the next valid
/// packet decodes.
#[test]
fn dvb_region_fills_and_resizes_pay_the_paint_budget() {
    // Region 0, fill flag set, 1024 wide, `height` tall, 4-bit, CLUT 0.
    let region = |height: u16| {
        let mut body = vec![0, 0x08, 0x04, 0x00];
        body.extend_from_slice(&height.to_be_bytes());
        body.extend_from_slice(&[0x08, 0, 0, 0]);
        segment(0x11, &body)
    };
    let same_size: Vec<_> = (0..1024).map(|_| region(1024)).collect();
    let resizes: Vec<_> = (0..1024).map(|i| region(1024 - i % 2)).collect();
    // A 16x16 page in a new epoch: one region at (0, 0) showing object 1,
    // 16 pixels of colour 1 (opaque in the default CLUT) on each field row.
    let page = segment(0x10, &[5, 0x18, 0, 0, 0, 0, 0, 0]);
    let small = segment(0x11, &[0, 0x08, 0, 16, 0, 16, 0x08, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
    let field = [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x00, 0xf0];
    let mut object = vec![0, 1, 0, 0, field.len() as u8, 0, field.len() as u8];
    object.extend_from_slice(&field);
    object.extend_from_slice(&field);
    let valid = dvb(&[page, small, segment(0x13, &object), segment(0x80, &[])]);
    for (what, segments) in [("1024 same-size fills", same_size), ("1024 resizes", resizes)] {
        let hostile = dvb(&segments);
        assert!(hostile.len() <= 16 * 1024 + 1, "{what}: {} bytes", hostile.len());
        let valid = valid.clone();
        within(Duration::from_secs(20), what, move || {
            let mut decoder = decoder("dvb_subtitle");
            assert!(decoder.send_packet(&packet(0, hostile)).is_err(), "{what}: 1 GiB of region writes in one packet");
            assert!(decoder.receive_frame().is_err(), "{what}: nothing to show");
            decoder.send_packet(&packet(90_000, valid)).unwrap();
            let Ok(Frame::Video(frame)) = decoder.receive_frame() else { panic!("{what}: the valid packet after it") };
            let plane = &frame.planes[0];
            assert_ne!(plane.data[3], 0, "{what}: the region's object is visible at (0, 0)");
        });
    }
}
