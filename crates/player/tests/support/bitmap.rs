//! Real FATE PGS display sets with explicit replacements/clears, decoded by
//! FFmpeg as the independent timing and complete-canvas oracle.

#[path = "../../../subs-bitmap/tests/support/mod.rs"]
pub mod oracle;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "peartube-subtitle-timing-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn ffmpeg(args: &[&str]) {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg on PATH");
    assert!(output.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn segment(out: &mut Vec<u8>, ms: u32, kind: u8, payload: &[u8]) {
    out.extend_from_slice(b"PG");
    out.extend_from_slice(&(ms * 90).to_be_bytes());
    out.extend_from_slice(&(ms * 90).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
}

/// FATE pgs_sub.sup ends partway through its second display set. Use its
/// complete FIRST display set (real palette, positions and RLE), repeated
/// at specified times, interspersed with empty PGS presentation sets. This
/// exercises an open-ended state beyond the old 2 s fallback, direct state
/// replacement without a spurious clear, and explicit clears after short
/// (< 500 ms) displays. FFmpeg decodes this exact stream as the oracle.
pub fn pgs_with_clears(path: &Path) {
    pgs_states(path, &[(200, true), (2500, true), (2800, false), (3000, true), (3400, false)]);
}

/// FATE pgs_sub.sup's first display set at each `(ms, true)` and an empty
/// presentation set (a clear) at each `(ms, false)`.
pub fn pgs_states(path: &Path, states: &[(u32, bool)]) {
    let source = std::fs::read(refcheck::fate("sub/pgs_sub.sup")).unwrap();
    let mut first = Vec::new();
    let mut cursor = 0;
    loop {
        assert_eq!(&source[cursor..cursor + 2], b"PG");
        let kind = source[cursor + 10];
        let length = u16::from_be_bytes(source[cursor + 11..cursor + 13].try_into().unwrap()) as usize;
        first.push((kind, source[cursor + 13..cursor + 13 + length].to_vec()));
        cursor += 13 + length;
        if kind == 0x80 {
            break;
        }
    }
    let mut clear = first.iter().find(|(kind, _)| *kind == 0x16).unwrap().1[..11].to_vec();
    clear[7] = 0; // normal presentation, not a new epoch
    clear[10] = 0; // zero composition objects: clear
    let mut data = Vec::new();
    for &(ms, visible) in states {
        if visible {
            for (kind, payload) in &first {
                segment(&mut data, ms, *kind, payload);
            }
        } else {
            segment(&mut data, ms, 0x16, &clear);
            segment(&mut data, ms, 0x80, &[]);
        }
    }
    std::fs::write(path, data).unwrap();
}

pub struct Show {
    pub at: Duration,
    pub width: usize,
    pub height: usize,
    pub canvas: Vec<u8>,
    pub blank: bool,
}

impl Show {
    /// Reconstruct the complete plane, including the positions of cropped
    /// images and every transparent pixel outside them.
    pub fn from_images<'a>(at: Duration, width: u32, height: u32, images: impl IntoIterator<Item = (i32, i32, u32, u32, &'a [u8])>) -> Self {
        let (width, height) = (width as usize, height as usize);
        let mut canvas = vec![0; width * height * 4];
        let mut blank = true;
        for (x, y, w, h, rgba) in images {
            blank = false;
            assert!(x >= 0 && y >= 0);
            assert!(x as usize + w as usize <= width);
            assert!(y as usize + h as usize <= height);
            for row in 0..h as usize {
                let src = row * w as usize * 4;
                let dst = ((y as usize + row) * width + x as usize) * 4;
                canvas[dst..dst + w as usize * 4].copy_from_slice(&rgba[src..src + w as usize * 4]);
            }
        }
        Self { at, width, height, canvas, blank }
    }

    pub fn assert_canvas(&self, reference: &oracle::Reference, index: usize) {
        let expected = &reference.cues[index];
        assert_eq!((self.width, self.height), (reference.width, reference.height), "event {index}: coordinate space");
        assert_eq!(self.blank, expected.sub.num_rects == 0, "event {index}: blank state");
        let diff = oracle::canvas_diff(&expected.canvas, &self.canvas, reference.width, oracle::Match::Visible);
        assert!(diff.is_none(), "event {index}: {diff:?}");
    }
}
