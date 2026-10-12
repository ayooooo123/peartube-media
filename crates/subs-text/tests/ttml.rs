use oxideav_core::{Frame, Segment};

fn plain(items: &[Segment]) -> String {
    items.iter().map(|s| match s {
        Segment::Text(s) | Segment::Raw(s) => s.clone(), Segment::LineBreak => "\n".into(),
        Segment::Bold(c) | Segment::Italic(c) | Segment::Underline(c) | Segment::Strike(c) => plain(c),
        Segment::Color { children, .. } | Segment::Font { children, .. } | Segment::Voice { children, .. } | Segment::Class { children, .. } | Segment::Karaoke { children, .. } => plain(children),
        Segment::Timestamp { .. } => String::new(),
    }).collect()
}

#[test]
fn inherited_timing_splits_timed_spans_and_preserves_styles() {
    let source = br#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><head><styling><style xml:id="red" tts:color="red" tts:fontStyle="italic"/></styling></head><body begin="1s"><div><p begin="1s" dur="3s" style="red">A<span begin="1s" dur="1s">B</span><br/>C</p></div></body></tt>"#;
    let cues = subs_text::ttml::decode(source).unwrap();
    assert_eq!(cues.iter().map(|c| (c.start_us,c.end_us,plain(&c.segments))).collect::<Vec<_>>(), [
        (2_000_000,3_000_000,"A\nC".into()), (3_000_000,4_000_000,"AB\nC".into()), (4_000_000,5_000_000,"A\nC".into()),
    ]);
    assert!(matches!(&cues[0].segments[0], Segment::Italic(c) if matches!(&c[0],Segment::Color { rgb: (255,0,0), .. })));
}

#[test]
fn sequential_cues_demux_one_at_a_time() {
    let source = br#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div timeContainer="seq"><p dur="1s">One</p><p dur="2s">Two</p></div></body></tt>"#;
    let context = codecs::context();
    let mut demux = subs_text::ttml::open_demuxer(Box::new(std::io::Cursor::new(source.to_vec())), &context.codecs).unwrap();
    let mut decoder = subs_text::ttml::make_decoder(&demux.streams()[0].params).unwrap();
    for (start,end,want) in [(0,1_000_000,"One"),(1_000_000,3_000_000,"Two")] {
        let packet = demux.next_packet().unwrap();
        assert_eq!(packet.pts,Some(start));
        decoder.send_packet(&packet).unwrap();
        let Frame::Subtitle(cue) = decoder.receive_frame().unwrap() else { panic!("text cue") };
        assert_eq!((cue.start_us,cue.end_us,plain(&cue.segments)),(start,end,want.to_owned()));
        assert!(matches!(decoder.receive_frame(),Err(oxideav_core::Error::NeedMore)));
    }
    assert!(matches!(demux.next_packet(),Err(oxideav_core::Error::Eof)));
}

#[test]
fn ticks_and_subframes_follow_the_declared_clock() {
    let ticks = br#"<tt xmlns="http://www.w3.org/ns/ttml"><body><p begin="2t" dur="3t">Ticks</p></body></tt>"#;
    let cues = subs_text::ttml::decode(ticks).unwrap();
    assert_eq!((cues[0].start_us, cues[0].end_us), (2_000_000, 5_000_000));
    let frames = br#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttp="http://www.w3.org/ns/ttml#parameter" ttp:frameRate="25" ttp:subFrameRate="10"><body><p begin="00:00:01:12.5" dur="125t">Subframes</p></body></tt>"#;
    let cues = subs_text::ttml::decode(frames).unwrap();
    assert_eq!((cues[0].start_us, cues[0].end_us), (1_500_000, 2_000_000));
}
