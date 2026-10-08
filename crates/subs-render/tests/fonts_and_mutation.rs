mod common;
use subs_render::{FontOptions, Renderer, Track, drawing, parse, shaper::{Shaper,Span,TextStyle}};

#[test]
fn real_fonts_shape_ligatures_bidi_and_fallback() {
    let mut shaper = Shaper::new(&common::options());
    let style = TextStyle { family: "DejaVu Sans".into(), size: 30.0, ..TextStyle::default() };
    let text = "office سلام हिन्दी";
    let spans = [Span { range: 0..text.len(), style }];
    let glyphs = shaper.shape(text,&spans,None,true);
    let office: Vec<_> = glyphs.iter().filter(|g| g.cluster < 6).collect();
    assert!(office.len() < "office".len(), "ffi is one ligature");
    assert!(glyphs.iter().any(|g| g.level.is_rtl() && !g.outline.is_empty()), "Arabic is shaped, not dropped");
    let hindi_start = text.find('ह').unwrap();
    assert!(glyphs.iter().filter(|g| g.cluster >= hindi_start).all(|g| !g.outline.is_empty()), "fallback covers Devanagari");
    let latin = shaper.fonts.select("DejaVu Sans",400,false,'A' as u32).unwrap();
    let hindi = shaper.fonts.select("DejaVu Sans",400,false,'ह' as u32).unwrap();
    assert_ne!(latin.id,hindi.id, "the missing glyph uses another face");
    assert!(hindi.meta.families.iter().any(|f| f == "Noto Sans Devanagari"));
    // A track font takes precedence over the same family on disk, and adding
    // it invalidates the old face/outline caches rather than leaving stale ids.
    shaper.add_font(std::fs::read(common::fonts().join("DejaVuSans.ttf")).unwrap().into());
    let embedded = shaper.fonts.select("DejaVu Sans",400,false,'A' as u32).unwrap();
    assert_eq!(embedded.id,0);
    assert_eq!(embedded.face().unwrap().glyph_index('A'),latin.face().unwrap().glyph_index('A'));
    let with_attachment = shaper.shape(text,&spans,None,true);
    assert_eq!(glyphs.iter().map(|g| g.advance).collect::<Vec<_>>(),with_attachment.iter().map(|g| g.advance).collect::<Vec<_>>());
}

#[test]
fn no_runtime_fonts_uses_the_last_resort_bitmap() {
    let mut shaper = Shaper::new(&FontOptions { directories: Some(Vec::new()), default_family: None });
    let style = TextStyle { size: 16.0, ..TextStyle::default() };
    let text = "AB\nC";
    let glyphs = shaper.shape(text,&[Span { range: 0..text.len(),style }],None,false);
    assert_eq!(glyphs.iter().map(|g|g.advance).collect::<Vec<_>>(),[8.0,8.0,0.0,8.0]);
    assert!(glyphs[2].outline.is_empty());
    assert_ne!(glyphs[0].outline.points,glyphs[1].outline.points);
}

fn next(state: &mut u64) -> u64 { *state ^= *state << 13; *state ^= *state >> 7; *state ^= *state << 17; *state }
fn mutate(seed: &[u8], state: &mut u64) -> Vec<u8> {
    let mut data = seed.to_vec();
    for _ in 0..=next(state) % 7 {
        let at = next(state) as usize % (data.len() + 1);
        match next(state) % 4 {
            0 if at < data.len() => data[at] = next(state) as u8,
            1 => data.insert(at,next(state) as u8),
            2 if at < data.len() => { data.remove(at); },
            _ => data.truncate(at),
        }
    }
    data
}

#[test]
fn seeded_ass_overrides_and_drawings_remain_bounded() {
    let mut track = Track::new();
    track.process_data(b"[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    let seeds: &[&[u8]] = &[
        br"{\an5\pos(100,100)\t(0,2000,\fscx130\frz45)\blur3\xbord2\alpha&H20&}AV {\r}text",
        br"{\move(1,2,100,200,0,1000)\fad(200,300)\clip(0,0,90,90)\kf40}ka{\ko40}raoke",
        br"{\p1}m 0 0 l 10 0 b 20 10 20 20 10 30 l 0 30{\p0}end",
        br"{\t(\t(\t(\t(\t(\blur99999999999)))))\fscx1e999\fs-1e999}x",
    ];
    let mut state = 0x5eed_cafe_7155_u64;
    for seed in seeds {
        for _ in 0..2000 {
            let event = subs_render::track::Event { start: 100, duration: 2000, text: mutate(seed,&mut state), ..Default::default() };
            if let Some(parsed) = parse::parse(&track,&event,800) {
                assert!(parsed.text.len() <= 64 << 10 && parsed.runs.len() <= 4096);
                for run in parsed.runs {
                    assert!(parsed.text.get(run.range).is_some(), "run must end on UTF-8 boundaries");
                    assert!(run.pen.text.size.is_finite() && run.pen.text.scale_x.is_finite() && run.pen.text.scale_y.is_finite());
                    assert!(run.pen.border.into_iter().all(|v| v.is_finite() && v >= 0.0));
                }
            }
        }
    }
    for seed in [b"m 0 0 l 100 0 100 80 0 80".as_slice(), b"m 0 0 b 0 100 100 100 100 0", b"m 0 0 s 10 10 20 0 30 10 p 40 0 c"] {
        for _ in 0..2000 {
            let data = mutate(seed,&mut state);
            if let Some((outline, _)) = drawing::parse(&data) {
                assert!(outline.points.len() <= 1 << 20);
                assert!(outline.points.iter().all(|p| p.x.abs() <= subs_render::outline::OUTLINE_MAX && p.y.abs() <= subs_render::outline::OUTLINE_MAX));
            }
        }
    }
}

#[test]
fn movement_fade_and_expiry_change_the_rendered_frame() {
    let mut track = Track::new();
    track.process_data(b"[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 240\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,{\\an5\\move(50,100,250,100)\\fad(500,500)}Moving\n");
    let mut renderer = Renderer::new(&common::options());
    let start = renderer.render(&mut track,0,320,240);
    assert_eq!(start.image.width,0, "fade starts transparent");
    let middle = renderer.render(&mut track,700,320,240);
    let later = renderer.render(&mut track,1200,320,240);
    assert!((later.image.x - middle.image.x - 50).abs() <= 1);
    assert!(middle.animated && middle.next_time == Some(2000));
    let end = renderer.render(&mut track,2000,320,240);
    assert_eq!(end.image.width,0, "end is exclusive");
    assert!(!end.animated && end.next_time.is_none());
}
