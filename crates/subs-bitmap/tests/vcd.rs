//! Structural protocol tests, NOT genuine-stream interoperability evidence.
//! Hand-authored packets exercise fragmentation, both fields, palette
//! conversion, sub-millisecond durations, truncated image data, colours
//! without a palette entry, unchecked packet numbering and regions leaving
//! the canvas; each complete canvas and interval is compared with original
//! VLC C. Archived CVD/OGT streams are a separate acceptance requirement.
mod support;
mod vlc_reference;
use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, RuntimeContext, TimeBase};
use parking_lot::Mutex;
use std::panic::AssertUnwindSafe;
static SERIAL: Mutex<()> = Mutex::new(());
const WIDTH: usize = 720;
const HEIGHT: usize = 576;
const TB: TimeBase = TimeBase::new(1, 1_000_000);

fn context() -> RuntimeContext { let mut ctx = RuntimeContext::new(); subs_bitmap::register(&mut ctx); ctx }
fn params(cvd: bool) -> CodecParameters {
    let mut params = CodecParameters::subtitle(CodecId::new(if cvd { subs_bitmap::CVD_CODEC_ID } else { subs_bitmap::OGT_CODEC_ID }));
    params.width = Some(WIDTH as u32); params.height = Some(HEIGHT as u32); params
}

/// One test image: position and size in stream coordinates, a pixel
/// pattern, and the protocol variations it exercises.
struct Image { x: u16, y: u16, width: u16, height: u16, duration: u32, truncate: bool, wide_colour_row: bool, numbering_gap: bool, fragments: usize }
fn colour(x: usize, y: usize) -> u8 { if (x + 2 * y) % 11 < 5 { 0 } else { (1 + (x / 3 + y) % 3) as u8 } }
fn bits_to_bytes(bits: &[u8]) -> Vec<u8> { bits.chunks(8).map(|byte| byte.iter().enumerate().fold(0, |v, (i, b)| v | (b << (7 - i)))).collect() }
fn push_bits(bits: &mut Vec<u8>, value: u8, count: usize) { for i in (0..count).rev() { bits.push((value >> i) & 1); } }
fn align(bits: &mut Vec<u8>) { while bits.len() % 8 != 0 { bits.push(0); } }

/// CVD rows, even field then odd field, as cvdsub.c RenderImage reads
/// them: runs of up to three as (count << 2 | colour), a zero nibble
/// filling the rest of the row with the next nibble's colour.
fn cvd_rle(image: &Image) -> Vec<u8> {
    let (w, h) = (usize::from(image.width), usize::from(image.height));
    let mut bits = Vec::new();
    for field in 0..2 {
        for y in (field..h).step_by(2) {
            if image.wide_colour_row && y == 0 { push_bits(&mut bits, 0, 4); push_bits(&mut bits, 7, 4); align(&mut bits); continue; }
            let mut x = 0;
            while x < w {
                let c = colour(x, y);
                let mut run = 1;
                while x + run < w && colour(x + run, y) == c { run += 1; }
                if x + run == w { push_bits(&mut bits, 0, 4); push_bits(&mut bits, c, 4); }
                else {
                    let mut left = run;
                    while left > 3 { push_bits(&mut bits, 12 | c, 4); left -= 3; }
                    push_bits(&mut bits, (left as u8) << 2 | c, 4);
                }
                x += run;
            }
            align(&mut bits);
        }
    }
    bits_to_bytes(&bits)
}
fn position(tag: u8, x: u16, y: u16) -> [u8; 4] { [tag, (x >> 6) as u8, ((x & 63) << 2) as u8 | (y >> 8) as u8, y as u8] }
fn cvd_spu(image: &Image) -> Vec<u8> {
    let mut rle = cvd_rle(image);
    // Lose the end of the image: VLC reads the following metadata as image
    // data, then zeros past the end of the unit.
    if image.truncate { rle.truncate(rle.len() * 3 / 5); }
    let mut body = vec![0; 4];
    body.extend(rle);
    let metadata = body.len() as u16;
    body.extend_from_slice(&[4, (image.duration >> 16) as u8, (image.duration >> 8) as u8, image.duration as u8]);
    body.extend_from_slice(&position(0x17, image.x, image.y));
    body.extend_from_slice(&position(0x1f, image.x + image.width - 1, image.y + image.height - 1));
    for (index, [y, u, v]) in [[16, 128, 128], [235, 128, 128], [80, 90, 240], [200, 255, 16]].into_iter().enumerate() { body.extend_from_slice(&[0x24 + index as u8, y, u, v]); }
    body.extend_from_slice(&[0x37, 0, 0x48, 0xf0]);
    body.extend_from_slice(&[0x47, 0, 0, 4]); body.extend_from_slice(&[0x4f, 0, 0, 22]);
    let size = (body.len() - 4) as u16;
    body[..2].copy_from_slice(&size.to_be_bytes()); body[2..4].copy_from_slice(&metadata.to_be_bytes()); body
}

/// OGT rows of one field as svcdsub.c reads them: two-bit colours, colour
/// 0 followed by (run - 1) in two bits.
fn ogt_field(image: &Image, field: usize) -> Vec<u8> {
    let (w, h) = (usize::from(image.width), usize::from(image.height));
    let mut bits = Vec::new();
    for y in (field..h).step_by(2) {
        let mut x = 0;
        while x < w {
            let c = colour(x, y);
            if c != 0 { push_bits(&mut bits, c, 2); x += 1; continue; }
            let mut run = 1;
            while x + run < w && run < 4 && colour(x + run, y) == 0 { run += 1; }
            push_bits(&mut bits, 0, 2); push_bits(&mut bits, run as u8 - 1, 2);
            x += run;
        }
        align(&mut bits);
    }
    bits_to_bytes(&bits)
}
fn ogt_spu(image: &Image) -> Vec<u8> {
    let mut body = vec![0, 0, if image.duration == 0 { 0x26 } else { 0x2e }, 0];
    if image.duration != 0 { body.extend_from_slice(&image.duration.to_be_bytes()); }
    for value in [image.x, image.y, image.width, image.height] { body.extend_from_slice(&value.to_be_bytes()); }
    for colour in [[16, 128, 128, 0], [235, 128, 128, 255], [80, 240, 90, 128], [200, 16, 255, 64]] { body.extend_from_slice(&colour); }
    body.push(0);
    let mut first = ogt_field(image, 0); if first.len() % 2 != 0 { first.push(0); }
    let mut second = ogt_field(image, 1);
    // Lose the end of the odd field: VLC reads zeros past the unit's end.
    if image.truncate { second.truncate(second.len() / 3); }
    body.extend_from_slice(&(first.len() as u16).to_be_bytes()); body.extend(first); body.extend(second);
    if !image.truncate { body.push(0); while body.len() % 4 != 0 { body.push(0); } }
    let size = body.len() as u16; body[..2].copy_from_slice(&size.to_be_bytes()); body
}

fn images() -> Vec<Image> {
    let base = Image { x: 40, y: 30, width: 23, height: 9, duration: 0, truncate: false, wide_colour_row: false, numbering_gap: false, fragments: 1 };
    vec![
        // Two packets, the first holding the whole header; 271011 µs.
        Image { duration: 24391, fragments: 2, ..base },
        // One packet, no duration: shown until the next subtitle.
        Image { x: 90, y: 60, ..base },
        // Truncated image data, a colour without palette entry (CVD) and
        // an unexpected OGT packet number that VLC only warns about.
        Image { y: 100, truncate: true, wide_colour_row: true, numbering_gap: true, fragments: 2, duration: 4500, ..base },
        // Leaves the canvas at its right and bottom edges (CVD x is
        // scaled by 3/4, so 955 places it at 716).
        Image { x: 714, y: 571, ..base },
    ]
}
fn packets(cvd: bool) -> Vec<Packet> {
    let mut packets = Vec::new();
    for (number, mut image) in images().into_iter().enumerate() {
        if cvd && image.x == 714 { image.x = 955; }
        if cvd && image.y + image.height > 1023 { image.y = 1023 - image.height; }
        let body = if cvd { cvd_spu(&image) } else { ogt_spu(&image) };
        let cut = if image.fragments == 2 { 40.min(body.len()) } else { body.len() };
        let chunks: Vec<&[u8]> = if image.fragments == 2 { vec![&body[..cut], &body[cut..]] } else { vec![&body] };
        for (fragment, chunk) in chunks.iter().enumerate() {
            let last = fragment + 1 == chunks.len();
            let sequence = if image.numbering_gap && fragment > 0 { fragment as u8 + 1 } else { fragment as u8 };
            let mut data = if cvd { vec![0] } else { vec![0x70, 0, sequence | if last { 0x80 } else { 0 }, 0, number as u8] };
            data.extend_from_slice(chunk);
            let mut packet = Packet::new(0, TB, data);
            if fragment == 0 { packet.pts = Some(1_000_000 + number as i64 * 1_000_000); }
            packets.push(packet);
        }
    }
    packets
}
fn compare(decoder: &mut dyn Decoder, packets: &[Packet], reference: &[vlc_reference::Cue]) {
    let mut count = 0;
    for packet in packets {
        decoder.send_packet(packet).unwrap();
        loop {
            match decoder.receive_frame() {
                Ok(frame) => { vlc_reference::assert_frame(frame, &reference[count], packet, WIDTH); count += 1; }
                Err(Error::NeedMore) => break,
                Err(error) => panic!("VCD receive: {error}"),
            }
        }
    }
    decoder.flush().unwrap(); assert!(matches!(decoder.receive_frame(), Err(Error::NeedMore)));
    assert_eq!(count, reference.len());
}
#[test]
fn cvd_and_ogt_structural_cases_match_original_vlc_full_canvases() {
    let _serial = SERIAL.lock();
    for cvd in [true, false] {
        let packets = packets(cvd);
        let reference = vlc_reference::reference(cvd, if cvd { "structural-cvd" } else { "structural-ogt" }, &packets, WIDTH, HEIGHT);
        assert_eq!(reference.len(), 4);
        assert_eq!(reference.iter().map(|cue| cue.duration.map(|d| d.as_micros())).collect::<Vec<_>>(), [Some(271011), None, Some(50000), None]);
        let visible = |cue: &vlc_reference::Cue| cue.canvas.chunks_exact(4).filter(|pixel| pixel[3] != 0).count();
        assert!(reference.iter().all(|cue| visible(cue) > 0), "every case must draw visible pixels");
        let mut decoder = context().codecs.first_decoder(&params(cvd)).unwrap();
        compare(decoder.as_mut(), &packets, &reference);
    }
}
fn random(seed: &mut u64) -> usize { *seed ^= *seed << 13; *seed ^= *seed >> 7; *seed ^= *seed << 17; *seed as usize }
fn drain(decoder: &mut dyn Decoder) {
    for _ in 0..8 {
        match decoder.receive_frame() {
            Ok(Frame::Video(frame)) => { assert_eq!(frame.image_planes().len(), 1); assert!(frame.image_planes()[0].data.len() <= 256 << 20); }
            Ok(_) => panic!("non-bitmap VCD frame"), Err(_) => return,
        }
    }
    panic!("unbounded VCD output");
}
#[test]
fn cvd_and_ogt_4800_structural_mutations_recover_against_original_vlc() {
    let _serial = SERIAL.lock();
    let ctx = context(); let mut seed = 0x5643_445f_4d55_5441;
    for cvd in [true, false] {
        let packets = packets(cvd);
        let reference = vlc_reference::reference(cvd, if cvd { "mutations-cvd-baseline" } else { "mutations-ogt-baseline" }, &packets, WIDTH, HEIGHT);
        for case in 0..2400 {
            let target = (case / 4) % packets.len();
            let mut mutant = packets[target].clone();
            if case % 4 == 0 { mutant.data.truncate(random(&mut seed) % mutant.data.len()); }
            else {
                for _ in 0..if case % 4 == 3 { 2 + random(&mut seed) % 7 } else { 1 } {
                    let at = random(&mut seed) % mutant.data.len(); mutant.data[at] ^= 1 << (random(&mut seed) % 8);
                }
                if mutant.data == packets[target].data { mutant.data[0] ^= 1; }
            }
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let mut decoder = ctx.codecs.first_decoder(&params(cvd)).unwrap();
                for packet in &packets[..target] { decoder.send_packet(packet).unwrap(); drain(decoder.as_mut()); }
                let _ = decoder.send_packet(&mutant); drain(decoder.as_mut());
                if let Some(next) = packets.get(target + 1) { let _ = decoder.send_packet(next); drain(decoder.as_mut()); }
                decoder.reset().unwrap();
                compare(decoder.as_mut(), &packets, &reference);
            }));
            assert!(result.is_ok(), "cvd={cvd} structural mutation={case} target={target}");
        }
    }
}
