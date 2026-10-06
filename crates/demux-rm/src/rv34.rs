// Ported from FFmpeg libavcodec/rv34_parser.c, commit 2da55bf
// License: GNU Lesser General Public License (LGPL) version 2.1 or later
//
// RealVideo 3.0/4.0 frame-type and 13-bit pts extraction from the picture
// header, plus the parser's pts correction: B-frame (type 3) container
// timestamps are rewritten relative to the last non-B frame, exactly as
// FFmpeg's rv34 parser does when the demuxer attaches it
// (AVSTREAM_PARSE_TIMESTAMPS, rmdec.c).

/// Frame types in the order of `rv_to_av_frame_type` (I, I, P, B).
const FRAME_TYPE_B: u32 = 3;

/// Per-stream parser state (`RV34ParseContext`).
#[derive(Default, Clone, Copy)]
pub struct Rv34ParserState {
    key_dts: i64,
    key_pts: i32,
    /// Seen at least one non-B frame (FFmpeg's `s->pts != AV_NOPTS_VALUE`
    /// guard on `pc->key_dts`).
    primed: bool,
}

impl Rv34ParserState {
    /// Correct the container pts of one assembled video frame.
    ///
    /// `codec_id` selects the header layout: rv30 and rv40 differ in the
    /// bit positions of the frame type and 13-bit pts
    /// (`rv34_parse`, libavcodec/rv34_parser.c). Returns the corrected pts.
    pub fn correct_pts(
        &mut self,
        codec_id: &str,
        container_pts: Option<i64>,
        data: &[u8],
    ) -> Option<i64> {
        // rv34_parse needs at least 13 + data[0] * 8 bytes.
        let slice0 = *data.first()? as usize;
        if data.len() < 13 + slice0 * 8 {
            return container_pts;
        }
        let hdr = u32::from_be_bytes([
            data[9 + slice0 * 8],
            data[10 + slice0 * 8],
            data[11 + slice0 * 8],
            data[12 + slice0 * 8],
        ]);
        let (frame_type, pts13) = if codec_id == "rv30" {
            (((hdr >> 27) & 3), ((hdr >> 7) & 0x1FFF) as i32)
        } else {
            (((hdr >> 29) & 3), ((hdr >> 6) & 0x1FFF) as i32)
        };

        let pts = container_pts.unwrap_or(i64::MIN);
        let out = if frame_type != FRAME_TYPE_B {
            // I/P frame: become the new reference (only when the container
            // gave us a pts to anchor on).
            if container_pts.is_some() {
                self.key_dts = pts;
                self.key_pts = pts13;
                self.primed = true;
            }
            pts
        } else if self.primed {
            // B frame: C computes `key_dts - ((key_pts - pts) & 0x1FFF)`
            // with wrapping i32 — the masked difference lands in 0..8191
            // and is subtracted.
            let delta = ((self
                .key_pts
                .wrapping_sub(pts13)) as u32
                & 0x1FFF) as i64;
            match self.key_dts.checked_sub(delta) {
                Some(v) => v,
                None => return container_pts,
            }
        } else {
            pts
        };
        Some(out)
    }
}
