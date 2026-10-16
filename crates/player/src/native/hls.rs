//! The native route's HLS playlists.

use super::types::VideoRange;

pub(crate) struct Variant<'a> {
    /// Peak bits per second.
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    /// RFC 6381 entries, video first.
    pub codecs: &'a [&'a str],
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    pub range: VideoRange,
}

pub(crate) fn master_playlist(variant: &Variant, media_uri: &str) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-STREAM-INF:BANDWIDTH=");
    out.push_str(&variant.bandwidth.to_string());

    if let Some(avg) = variant.average_bandwidth {
        out.push_str(",AVERAGE-BANDWIDTH=");
        out.push_str(&avg.to_string());
    }

    out.push_str(",CODECS=\"");
    out.push_str(&variant.codecs.join(","));
    out.push('"');

    if let Some((w, h)) = variant.resolution {
        out.push_str(&format!(",RESOLUTION={}x{}", w, h));
    }

    if let Some(fps) = variant.frame_rate {
        out.push_str(&format!(",FRAME-RATE={:.3}", fps));
    }

    out.push_str(",VIDEO-RANGE=");
    match variant.range {
        VideoRange::Sdr => out.push_str("SDR"),
        VideoRange::Pq => out.push_str("PQ"),
        VideoRange::Hlg => out.push_str("HLG"),
    }
    out.push('\n');

    out.push_str(media_uri);
    out.push('\n');

    out
}

/// VOD media playlist. `positions_ms`: segment start positions on the item axis in ms, then the end; len = segments + 1, strictly increasing, positions_ms[0] == 0 (panic in debug otherwise).
pub(crate) fn media_playlist(
    positions_ms: &[u64],
    init_uri: &str,
    segment_uri: &dyn Fn(usize) -> String,
) -> String {
    debug_assert!(
        positions_ms.len() >= 2,
        "positions_ms must have at least 2 entries (start and end)"
    );
    debug_assert_eq!(positions_ms[0], 0, "positions_ms[0] must be 0");
    debug_assert!(
        positions_ms.windows(2).all(|w| w[0] < w[1]),
        "positions_ms must be strictly increasing"
    );

    if positions_ms.len() < 2 {
        return String::new();
    }

    let segment_count = positions_ms.len() - 1;

    let target_duration = (0..segment_count)
        .map(|i| {
            let dur = positions_ms[i + 1] - positions_ms[i];
            (dur + 999) / 1000
        })
        .max()
        .unwrap_or(0);

    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{}\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-MAP:URI=\"{}\"\n",
        target_duration, init_uri
    );

    for i in 0..segment_count {
        let dur = positions_ms[i + 1] - positions_ms[i];
        let s = dur / 1000;
        let mmm = dur % 1000;
        out.push_str(&format!("#EXTINF:{}.{:03},\n", s, mmm));
        out.push_str(&segment_uri(i));
        out.push('\n');
    }

    out.push_str("#EXT-X-ENDLIST\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_master_playlist_full() {
        let variant = Variant {
            bandwidth: 5_000_000,
            average_bandwidth: Some(4_500_000),
            codecs: &["avc1.64001f", "mp4a.40.2"],
            resolution: Some((1280, 720)),
            frame_rate: Some(23.976),
            range: VideoRange::Sdr,
        };
        let m3u8 = master_playlist(&variant, "media.m3u8");
        let expected = "#EXTM3U\n\
                        #EXT-X-VERSION:7\n\
                        #EXT-X-INDEPENDENT-SEGMENTS\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=5000000,AVERAGE-BANDWIDTH=4500000,CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=1280x720,FRAME-RATE=23.976,VIDEO-RANGE=SDR\n\
                        media.m3u8\n";
        assert_eq!(m3u8, expected);
    }

    #[test]
    fn test_master_playlist_minimal_pq() {
        let variant = Variant {
            bandwidth: 10_000_000,
            average_bandwidth: None,
            codecs: &["hvc1.2.4.L153.B0"],
            resolution: None,
            frame_rate: None,
            range: VideoRange::Pq,
        };
        let m3u8 = master_playlist(&variant, "hdr.m3u8");
        let expected = "#EXTM3U\n\
                        #EXT-X-VERSION:7\n\
                        #EXT-X-INDEPENDENT-SEGMENTS\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=10000000,CODECS=\"hvc1.2.4.L153.B0\",VIDEO-RANGE=PQ\n\
                        hdr.m3u8\n";
        assert_eq!(m3u8, expected);
    }

    #[test]
    fn test_master_playlist_hlg() {
        let variant = Variant {
            bandwidth: 8_000_000,
            average_bandwidth: None,
            codecs: &["hvc1.2.4.L153.B0"],
            resolution: Some((1920, 1080)),
            frame_rate: Some(50.0),
            range: VideoRange::Hlg,
        };
        let m3u8 = master_playlist(&variant, "hlg.m3u8");
        let expected = "#EXTM3U\n\
                        #EXT-X-VERSION:7\n\
                        #EXT-X-INDEPENDENT-SEGMENTS\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS=\"hvc1.2.4.L153.B0\",RESOLUTION=1920x1080,FRAME-RATE=50.000,VIDEO-RANGE=HLG\n\
                        hlg.m3u8\n";
        assert_eq!(m3u8, expected);
    }

    #[test]
    fn test_media_playlist_3_segments_exact_text() {
        let positions_ms = [0, 4000, 8001, 12000];
        let init_uri = "init.mp4";
        let pl = media_playlist(&positions_ms, init_uri, &|i| format!("seg{}.m4s", i));

        let expected = "#EXTM3U\n\
                        #EXT-X-VERSION:7\n\
                        #EXT-X-TARGETDURATION:5\n\
                        #EXT-X-PLAYLIST-TYPE:VOD\n\
                        #EXT-X-MEDIA-SEQUENCE:0\n\
                        #EXT-X-INDEPENDENT-SEGMENTS\n\
                        #EXT-X-MAP:URI=\"init.mp4\"\n\
                        #EXTINF:4.000,\n\
                        seg0.m4s\n\
                        #EXTINF:4.001,\n\
                        seg1.m4s\n\
                        #EXTINF:3.999,\n\
                        seg2.m4s\n\
                        #EXT-X-ENDLIST\n";
        assert_eq!(pl, expected);

        // Verify durations sum to the exact end
        let durations: Vec<u64> = (0..positions_ms.len() - 1)
            .map(|i| positions_ms[i + 1] - positions_ms[i])
            .collect();
        let sum: u64 = durations.iter().sum();
        assert_eq!(sum, 12000);
        assert_eq!(sum, *positions_ms.last().unwrap());
    }

    #[test]
    fn test_target_duration_with_4001ms_segment_is_5() {
        let positions = [0, 4001];
        let pl = media_playlist(&positions, "init.mp4", &|_| "seg0.m4s".to_string());
        assert!(pl.contains("#EXT-X-TARGETDURATION:5\n"));

        let positions_4000 = [0, 4000];
        let pl_4000 = media_playlist(&positions_4000, "init.mp4", &|_| "seg0.m4s".to_string());
        assert!(pl_4000.contains("#EXT-X-TARGETDURATION:4\n"));

        let positions_1 = [0, 1];
        let pl_1 = media_playlist(&positions_1, "init.mp4", &|_| "seg0.m4s".to_string());
        assert!(pl_1.contains("#EXT-X-TARGETDURATION:1\n"));
    }

    #[test]
    #[should_panic]
    fn test_media_playlist_debug_assert_starts_with_0() {
        let positions = [100, 4000];
        let _ = media_playlist(&positions, "init.mp4", &|_| "seg0.m4s".to_string());
    }

    #[test]
    #[should_panic]
    fn test_media_playlist_debug_assert_strictly_increasing() {
        let positions = [0, 4000, 4000];
        let _ = media_playlist(&positions, "init.mp4", &|_| "seg0.m4s".to_string());
    }
}
