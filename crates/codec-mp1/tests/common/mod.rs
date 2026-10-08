//! Random valid Layer I frames: random bit allocations up to 15 (dropped
//! until the frame holds them), scale factors up to 63, mantissas, CRC
//! words, padding bits, mode extensions, then ancillary bytes. Each test
//! binary uses part of it.

#![allow(dead_code)]

/// Layer I bitrates in kbit/s: MPEG-1, then MPEG-2 and 2.5.
const KBPS: [[u32; 15]; 2] = [
    [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
];

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n.max(1))) as u32
    }
}

#[derive(Default)]
struct Bits {
    bits: Vec<bool>,
}

impl Bits {
    fn put(&mut self, v: u32, n: u32) {
        self.bits.extend((0..n).rev().map(|i| (v >> i) & 1 != 0));
    }

    fn bytes(&self) -> Vec<u8> {
        self.bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &x)| b | (u8::from(x) << (7 - i)))).collect()
    }
}

/// CRC-16 (x^16 + x^15 + x^2 + 1, from 0xFFFF) over `bits`.
fn crc16(bits: &[bool]) -> u32 {
    let mut crc = 0xFFFFu32;
    for &b in bits {
        let top = (crc >> 15) & 1 != 0;
        crc = (crc << 1) & 0xFFFF;
        if top != b {
            crc ^= 0x8005;
        }
    }
    crc
}

#[derive(Clone, Copy)]
pub enum Version {
    Mpeg1,
    Mpeg2,
    Mpeg25,
}

#[derive(Clone, Copy)]
pub struct Spec {
    pub version: Version,
    pub rate_index: u32,
    pub bitrate_index: u32,
    /// 0 stereo, 1 joint stereo, 2 dual channel, 3 mono.
    pub mode: u32,
    pub crc: bool,
    pub frames: usize,
    /// Bytes cut from the end of the stream (a cut last frame).
    pub cut: usize,
}

impl Spec {
    pub fn sample_rate(&self) -> u32 {
        let shift = match self.version {
            Version::Mpeg1 => 0,
            Version::Mpeg2 => 1,
            Version::Mpeg25 => 2,
        };
        [44100, 48000, 32000][self.rate_index as usize] >> shift
    }

    pub fn channels(&self) -> u16 {
        if self.mode == 3 { 1 } else { 2 }
    }
}

/// One frame of `s`.
pub fn frame(rng: &mut Rng, s: &Spec) -> Vec<u8> {
    let (id_bits, lsf) = match s.version {
        Version::Mpeg1 => (0b11, 0),
        Version::Mpeg2 => (0b10, 1),
        Version::Mpeg25 => (0b00, 1),
    };
    let padding = rng.below(2);
    let mode_ext = rng.below(4);
    let size = ((KBPS[lsf][s.bitrate_index as usize] * 12000 / s.sample_rate() + padding) * 4) as usize;
    let header = 0xFFE0_0000
        | id_bits << 19
        | 0b11 << 17
        | u32::from(!s.crc) << 16
        | s.bitrate_index << 12
        | s.rate_index << 10
        | padding << 9
        | s.mode << 6
        | mode_ext << 4;
    let nch = usize::from(s.channels());
    let bound = if s.mode == 1 { (mode_ext as usize + 1) * 4 } else { 32 };

    let mut alloc = [[0u32; 32]; 2];
    for sb in 0..32 {
        for ch in 0..if sb < bound { nch } else { 1 } {
            alloc[ch][sb] = if rng.below(3) == 0 { 0 } else { 1 + rng.below(15) };
        }
    }
    let alloc_bits = (bound * nch + (32 - bound)) * 4;
    let cost = |alloc: &[[u32; 32]; 2]| -> usize {
        (0..32)
            .map(|sb| {
                if sb < bound {
                    (0..nch).filter(|&ch| alloc[ch][sb] != 0).map(|ch| 6 + 12 * (alloc[ch][sb] as usize + 1)).sum()
                } else if alloc[0][sb] != 0 {
                    12 + 12 * (alloc[0][sb] as usize + 1)
                } else {
                    0
                }
            })
            .sum()
    };
    let budget = size * 8 - 32 - if s.crc { 16 } else { 0 } - alloc_bits;
    while cost(&alloc) > budget {
        let sb = rng.below(32) as usize;
        let ch = rng.below(nch as u32) as usize;
        alloc[ch][sb] = 0;
    }

    let mut b = Bits::default();
    b.put(header, 32);
    if s.crc {
        b.put(0, 16);
    }
    let alloc_start = b.bits.len();
    for sb in 0..bound {
        for ch in 0..nch {
            b.put(alloc[ch][sb], 4);
        }
    }
    for sb in bound..32 {
        b.put(alloc[0][sb], 4);
    }
    if s.crc {
        let mut covered = b.bits[16..32].to_vec();
        covered.extend_from_slice(&b.bits[alloc_start..]);
        let crc = crc16(&covered);
        for i in 0..16 {
            b.bits[32 + i] = (crc >> (15 - i)) & 1 != 0;
        }
    }
    for sb in 0..32 {
        let chans = if sb < bound { nch } else if alloc[0][sb] != 0 { 2 } else { 0 };
        for ch in 0..chans {
            if alloc[if sb < bound { ch } else { 0 }][sb] != 0 {
                // Scale factor 63 (outside the standard's table) included.
                b.put(rng.below(64), 6);
            }
        }
    }
    for _ in 0..12 {
        for sb in 0..32 {
            for ch in 0..if sb < bound { nch } else { 1 } {
                let n = alloc[ch][sb];
                if n != 0 {
                    b.put(rng.below(1 << (n + 1)), n + 1);
                }
            }
        }
    }
    let mut out = b.bytes();
    assert!(out.len() <= size, "frame overflow");
    while out.len() < size {
        out.push(rng.next() as u8);
    }
    out
}

/// The frames of `s` from `seed`, one after another, cut as `s.cut` says.
pub fn stream(seed: u64, s: &Spec) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut data: Vec<u8> = (0..s.frames).flat_map(|_| frame(&mut rng, s)).collect();
    data.truncate(data.len() - s.cut);
    data
}
