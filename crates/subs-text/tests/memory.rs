//! A cue's decoded form must stay proportional to its input. Every
//! allocation in this test binary is counted (one test, so nothing else
//! allocates alongside), and each crafted input below, a few KiB to a few
//! dozen KiB, must decode within a few MiB of peak memory through the
//! production registry, with its text intact.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use oxideav_core::{CodecId, CodecParameters, Frame, Packet, Segment, SubtitleCue, TimeBase};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let now = CURRENT.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(now, Relaxed);
}

// SAFETY: every call forwards to the system allocator unchanged; the
// counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            CURRENT.fetch_sub(layout.size(), Relaxed);
            grew(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const LIMIT: usize = 8 << 20;

/// Decodes `packet` with a decoder for `params` built by the production
/// registry; returns the cue and the peak memory the decoding needed
/// beyond what was live before it.
fn decode_measured(ctx: &oxideav_core::RuntimeContext, params: &CodecParameters, packet: &Packet) -> (SubtitleCue, usize) {
    let base = CURRENT.load(Relaxed);
    PEAK.store(base, Relaxed);
    let cue = {
        let mut decoder = ctx.codecs.first_decoder(params).unwrap();
        decoder.send_packet(packet).unwrap();
        match decoder.receive_frame() {
            Ok(Frame::Subtitle(cue)) => cue,
            other => panic!("decoded to {other:?}"),
        }
    };
    (cue, PEAK.load(Relaxed).saturating_sub(base))
}

fn shown(segments: &[Segment], out: &mut String) {
    for segment in segments {
        match segment {
            Segment::Text(t) | Segment::Raw(t) => out.push_str(t),
            Segment::LineBreak => out.push('\n'),
            Segment::Bold(c) | Segment::Italic(c) | Segment::Underline(c) | Segment::Strike(c)
            | Segment::Color { children: c, .. } | Segment::Font { children: c, .. }
            | Segment::Voice { children: c, .. } | Segment::Class { children: c, .. }
            | Segment::Karaoke { children: c, .. } => shown(c, out),
            Segment::Timestamp { .. } => {}
        }
    }
}

fn text_of(cue: &SubtitleCue) -> String {
    let mut out = String::new();
    shown(&cue.segments, &mut out);
    out
}

fn ass(header_font: &str, event: &str) -> (CodecParameters, Packet) {
    let mut params = CodecParameters::subtitle(CodecId::new("ass"));
    params.extradata = format!(
        "[V4+ Styles]\nFormat: Name, Fontname, PrimaryColour\nStyle: Default,Arial,&H00FFFFFF\nStyle: Big,{header_font},&H0000FFFF\n"
    )
    .into_bytes();
    let packet = Packet::new(0, TimeBase::new(1, 1000), event.as_bytes().to_vec()).with_pts(0).with_duration(1000);
    (params, packet)
}

/// A tx3g sample entry whose font table holds `fonts` entries, all font
/// id 2 named "A" (the default style uses font id 1), and a sample of
/// `chars` x's with a style record on every other character using font 2.
fn mov_text(fonts: u16, chars: u16) -> (CodecParameters, Packet) {
    let mut extradata = vec![0, 0, 0, 0, 1, 0xff, 0, 0, 0, 0];
    extradata.extend_from_slice(&[0; 8]); // BoxRecord
    extradata.extend_from_slice(&[0, 0, 0, 0]); // StyleRecord start/end
    extradata.extend_from_slice(&[0, 1, 0, 18, 0xff, 0xff, 0xff, 0xff]); // font 1, size 18, white, opaque
    extradata.extend_from_slice(&[0, 0, 0, 0, b'f', b't', b'a', b'b']);
    extradata.extend_from_slice(&fonts.to_be_bytes());
    for _ in 0..fonts {
        extradata.extend_from_slice(&[0, 2, 1, b'A']);
    }
    let mut params = CodecParameters::subtitle(CodecId::new("mov_text"));
    params.extradata = extradata;
    let styles = chars / 2;
    let mut sample = chars.to_be_bytes().to_vec();
    sample.extend(std::iter::repeat_n(b'x', usize::from(chars)));
    sample.extend_from_slice(&(10 + 12 * u32::from(styles)).to_be_bytes());
    sample.extend_from_slice(b"styl");
    sample.extend_from_slice(&styles.to_be_bytes());
    for i in 0..styles {
        sample.extend_from_slice(&(2 * i).to_be_bytes());
        sample.extend_from_slice(&(2 * i + 1).to_be_bytes());
        sample.extend_from_slice(&[0, 2, 0, 18, 0xff, 0xff, 0xff, 0xff]);
    }
    (params, Packet::new(0, TimeBase::new(1, 1000), sample).with_pts(0).with_duration(1000))
}

#[test]
fn decoded_cues_stay_proportional_to_their_input() {
    let ctx = codecs::context();
    let big_font = "F".repeat(32 << 10);

    // A style with a huge font name, shown in 2048 runs and line breaks.
    let (params, packet) = ass(&big_font, &format!("0,0,Big,,0,0,0,,{}", "x\\N".repeat(1024)));
    let (cue, peak) = decode_measured(&ctx, &params, &packet);
    assert_eq!(text_of(&cue), "x\n".repeat(1024));
    assert!(peak < LIMIT, "huge style font over 2048 runs: {peak} bytes");

    // Overrides switching away from and back to that style.
    let (params, packet) = ass(&big_font, &format!("0,0,Big,,0,0,0,,{}", "{\\fnA}x{\\r}y".repeat(1024)));
    let (cue, peak) = decode_measured(&ctx, &params, &packet);
    assert_eq!(text_of(&cue), "xy".repeat(1024));
    assert!(peak < LIMIT, "\\r back to a huge style 1024 times: {peak} bytes");

    // Resets naming the huge style from another style's event.
    let (params, packet) = ass(&big_font, &format!("0,0,Default,,0,0,0,,{}", "{\\rBig}x{\\fnA}y".repeat(1024)));
    let (cue, peak) = decode_measured(&ctx, &params, &packet);
    assert_eq!(text_of(&cue), "xy".repeat(1024));
    assert!(peak < LIMIT, "\\rBig 1024 times: {peak} bytes");

    // A mov_text font table repeating one font id 4096 times, used by
    // 2048 style records.
    let (params, packet) = mov_text(4096, 4096);
    let (cue, peak) = decode_measured(&ctx, &params, &packet);
    assert_eq!(text_of(&cue), "x".repeat(4096));
    assert!(peak < LIMIT, "mov_text repeated font ids: {peak} bytes");
}
