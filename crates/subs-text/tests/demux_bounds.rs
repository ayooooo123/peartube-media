use oxideav_core::{Error, Packet, ProbeData};

fn packets(format: &str, text: &str) -> Vec<Packet> {
    let ctx = codecs::context();
    let mut demux = ctx.containers.open_demuxer(
        format, Box::new(std::io::Cursor::new(text.as_bytes().to_vec())), &ctx.codecs,
    ).unwrap();
    let mut packets = Vec::new();
    loop {
        match demux.next_packet() {
            Ok(packet) => packets.push(packet),
            Err(Error::Eof) => return packets,
            Err(error) => panic!("{format}: {error}"),
        }
    }
}

#[test]
fn unrepresentable_timestamp_components_do_not_wrap_into_cues() {
    let subviewer = "[DELAY]\n1\n[3000000000000000:00:00]\nbad hour\n[00:3000000000000000:00]\nbad minute\n[00:00:3000000000000000]\nbad second\n[DELAY]\n3000000000000000\n[00:00:02]\nvalid\n";
    let actual = packets("subviewer1", subviewer);
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].pts, Some(3));
    assert_eq!(actual[0].data, b"valid");

    let vplayer = "3000000000000000:00:00:bad hour\n00:3000000000000000:00:bad minute\n00:00:3000000000000000:bad second\n00:00:00.3000000000000000:bad fraction\n00:00:02:valid\n";
    assert_eq!(subs_text::vplayer::probe(&ProbeData { buf: vplayer.as_bytes(), ext: None }), 0);
    let actual = packets("vplayer", vplayer);
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].pts, Some(200));
    assert_eq!(actual[0].data, b"valid");
}

#[test]
fn chronological_cues_keep_only_exact_duration_duplicates() {
    let inputs = [
        ("subviewer1", "[DELAY]\n0\n[00:00:03]\nlate\n[00:00:04]\n\n[00:00:01]\nearly\n[00:00:02]\nsame\n[00:00:02]\nsame\n[00:00:02]\nsame\n", 1, Some(1)),
        ("vplayer", "00:00:03:late\n00:00:01:early\n00:00:02:same\n00:00:02:same\n00:00:02:same\n", 100, Some(-1)),
        ("sami", "<SAMI><BODY>\n<SYNC Start=3000><P>late\n<SYNC Start=1000><P>early\n<SYNC Start=2000><P>same\n<SYNC Start=2000><P>same\n<SYNC Start=2000><P>same\n</BODY></SAMI>", 1000, None),
    ];
    for (format, text, scale, final_duration) in inputs {
        let actual = packets(format, text);
        let intervals: Vec<_> = actual.iter().map(|p| (p.pts, p.duration)).collect();
        // FFmpeg fills durations before removing adjacent exact duplicates:
        // equal-text cues with different durations must remain distinct.
        assert_eq!(intervals, vec![
            (Some(scale), Some(scale)), (Some(2 * scale), Some(0)),
            (Some(2 * scale), Some(scale)), (Some(3 * scale), final_duration),
        ], "{format}");
        assert!(actual[0].data.ends_with(b"early") || actual[0].data.ends_with(b"early\n"));
        assert!(actual[3].data.ends_with(b"late") || actual[3].data.ends_with(b"late\n"));
    }
}
