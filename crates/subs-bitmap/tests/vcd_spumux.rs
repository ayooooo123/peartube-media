//! CVD and SVCD OGT streams authored by an independent encoder, dvdauthor
//! 0.7.2 `spumux` (tests/data/spumux/generate.sh), read by the production
//! MPEG-PS demuxer and compared with original VLC C. These are third-party
//! authored streams, not archived disc rips, which remain unavailable.
mod support;
mod vlc_reference;

use std::io::Cursor;
use std::path::Path;
use oxideav_core::{Error, MediaType, RuntimeContext};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/spumux");

#[test]
fn spumux_svcd_and_cvd_program_streams_match_original_vlc() {
    for (cvd, name, md5, fragments, expected_us) in [
        // spumux durations: 1.5 s, until the next subtitle less 1 ms, 1 s.
        (false, "svcd-spumux.mpg", "01564ea9019d757a0afb5d2cfe31580a", [3, 1, 1], [1_500_000, 999_000, 1_000_000]),
        // spumux appends 04 08 0c 10 after the SPU size it records; VLC
        // counts those bytes and reads them as a later duration field.
        (true, "cvd-spumux.mpg", "81cedf7702b4735feaf56b3321e83801", [3, 1, 1], [5_859_733; 3]),
    ] {
        let bytes = std::fs::read(Path::new(DATA).join(name)).unwrap();
        assert_eq!(refcheck::md5_hex(&bytes), md5, "{name}: fixture provenance");
        let mut ctx = RuntimeContext::new();
        subs_bitmap::register(&mut ctx);
        demux_misc::register(&mut ctx);
        let mut demux = ctx.containers.open_demuxer("mpeg", Box::new(Cursor::new(bytes)), &ctx.codecs).unwrap();
        let streams = demux.streams().to_vec();
        let video = streams.iter().find(|s| s.params.media_type == MediaType::Video).expect("video stream");
        let (width, height) = (video.params.width.unwrap() as usize, video.params.height.unwrap() as usize);
        let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).expect("subtitle stream at open").clone();
        assert_eq!(subtitle.params.codec_id.as_str(), if cvd { subs_bitmap::CVD_CODEC_ID } else { subs_bitmap::OGT_CODEC_ID });
        let mut packets = Vec::new();
        loop {
            match demux.next_packet() {
                Ok(packet) if packet.stream_index == subtitle.index => packets.push(packet),
                Ok(_) => {}
                Err(Error::Eof) => break,
                Err(error) => panic!("{name}: {error}"),
            }
        }
        // Continuations carry no PTS; each subtitle starts with one.
        let starts: Vec<usize> = packets.iter().enumerate().filter(|(_, p)| p.pts.is_some()).map(|(i, _)| i).collect();
        let spans: Vec<usize> = starts.iter().zip(starts.iter().skip(1).chain([&packets.len()])).map(|(a, b)| b - a).collect();
        assert_eq!(spans, fragments, "{name}: private-stream packets per subtitle");
        let reference = vlc_reference::reference(cvd, name, &packets, width, height);
        assert_eq!(reference.iter().map(|cue| cue.duration.unwrap().as_micros()).collect::<Vec<_>>(), expected_us, "{name}: VLC intervals");
        assert!(reference.iter().all(|cue| cue.canvas.chunks_exact(4).any(|pixel| pixel[3] != 0)), "{name}: visible subtitles");
        // VLC positions these regions in video pixels, so the decoder's
        // canvas is the video's (FFmpeg's sub2video fallback rule too).
        let mut params = subtitle.params.clone();
        params.width = Some(width as u32);
        params.height = Some(height as u32);
        let mut decoder = ctx.codecs.first_decoder(&params).unwrap();
        let mut count = 0;
        for packet in &packets {
            decoder.send_packet(packet).unwrap_or_else(|error| panic!("{name}: {error}"));
            while let Ok(frame) = decoder.receive_frame() {
                vlc_reference::assert_frame(frame, &reference[count], packet, width);
                count += 1;
            }
        }
        assert_eq!(count, reference.len(), "{name}: subtitle count");
        eprintln!("{name}: {} packets, {count} complete {width}x{height} canvases and intervals equal original VLC C", packets.len());
    }
}
