//! Deterministic no-panic test for the subtitle composition math (the
//! contract's untrusted-input rule): fixed seed, >= 2000 mutations of the
//! reference input, no panics, and hostile coordinates stay inside the
//! destination buffer.

use player::backend::SubtitleImage;
use player::subtitle_compose::compose_straight;

/// xorshift64* — deterministic, no external crate.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn base_images() -> Vec<SubtitleImage> {
    vec![
        SubtitleImage {
            x: 10,
            y: 5,
            width: 8,
            height: 4,
            rgba: [200, 100, 50, 128].repeat(8 * 4),
        },
        SubtitleImage {
            x: -3,
            y: -2,
            width: 5,
            height: 5,
            rgba: [0, 255, 0, 200].repeat(5 * 5),
        },
    ]
}

fn base_dims() -> (usize, usize, u32) {
    (32 * 4, 16, 32) // stride px, height, video w
}

#[test]
fn subtitle_compose_survives_2000_mutations() {
    let (stride, height, vw) = base_dims();
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut count = 0;

    for _ in 0..2200 {
        // Fresh baseline copy per mutation.
        let mut images = base_images();
        // Destination: sometimes a deliberately hostile (short) buffer.
        let dst_len = if rng.below(4) == 0 {
            rng.below((stride * height) as u64) as usize
        } else {
            stride * height
        };
        let mut dst = vec![0u8; dst_len];
        let stride_m = if rng.below(4) == 0 {
            rng.below(64) as usize
        } else {
            stride
        };
        let height_m = if rng.below(4) == 0 {
            rng.below(64) as usize
        } else {
            height
        };
        let vw_m = if rng.below(4) == 0 {
            rng.below(200) as u32
        } else {
            vw
        };

        // Mutate the images.
        let which = rng.below(images.len() as u64) as usize;
        match rng.below(10) {
            0 => images[which].x = rng.next() as i32, // includes i32::MAX
            1 => images[which].y = (rng.next() as i64 - i64::from(i32::MAX)) as i32,
            2 => images[which].x = i32::MAX - rng.below(3) as i32,
            3 => images[which].y = i32::MIN + rng.below(3) as i32,
            4 => images[which].width = rng.below(4096) as u32,
            5 => images[which].height = rng.below(4096) as u32,
            6 => images[which].rgba.truncate(rng.below(200) as usize),
            7 => images[which].rgba.push(rng.next() as u8),
            8 => {
                images[which] = SubtitleImage {
                    x: i32::MAX,
                    y: 0,
                    width: 2,
                    height: 1,
                    rgba: [255, 0, 0, 255].repeat(2), // the 2x1 overflow case
                };
            }
            _ => images[which].x = -(rng.below(i32::MAX as u64) as i32),
        }

        // Also mutate a copy's rgba length independently of width*height.
        if rng.below(3) == 0 {
            images[which].rgba = [10, 20, 30, 40].repeat(rng.below(100) as usize % 200);
        }

        compose_straight(&mut dst, stride_m, height_m, &images, vw_m);
        count += 1;
    }
    assert!(count >= 2000, "ran {count} mutations");
}

#[test]
fn subtitle_compose_bounds_stay_in_buffer() {
    // The exact overflow case from the packet: 2x1 image at x=i32::MAX.
    let (stride, height) = (32usize * 4, 16usize);
    let mut dst = vec![0u8; stride * height];
    let images = [SubtitleImage {
        x: i32::MAX,
        y: 0,
        width: 2,
        height: 1,
        rgba: [255, 0, 0, 255].repeat(2),
    },
    ];
    compose_straight(&mut dst, stride, height, &images, 32);
    // Nothing drawn: x + col overflows past any sane target.
    assert!(dst.iter().all(|&b| b == 0));
}

#[test]
fn subtitle_compose_overwrites_inside_bounds() {
    let (stride, height) = (32usize * 4, 16usize);
    let mut dst = vec![0u8; stride * height];
    let images = [SubtitleImage {
        x: 4,
        y: 2,
        width: 2,
        height: 2,
        rgba: [255, 0, 0, 255].repeat(2 * 2),
    },
    ];
    compose_straight(&mut dst, stride, height, &images, 32);
    // Fully-opaque red must have landed at (4,2)..(5,3).
    for y in 2..4 {
        for x in 4..6 {
            let i = (y * stride + x) * 4;
            assert_eq!(&dst[i..i + 4], &[255, 0, 0, 255]);
        }
    }
}
