//! Compact LZSS-packed RV30/RV40 VLC code-length tables.
//!
//! Generated from FFmpeg libavcodec/rv34vlc.h (commit 2da55bf), LGPL-2.1-or-later.

pub const NUM_INTRA_TABLES: usize = 5;
pub const NUM_INTER_TABLES: usize = 7;
pub const CBPPAT_VLC_SIZE: usize = 1296;
pub const CBP_VLC_SIZE: usize = 16;
pub const FIRSTBLK_VLC_SIZE: usize = 864;
pub const OTHERBLK_VLC_SIZE: usize = 108;
pub const COEFF_VLC_SIZE: usize = 32;

const RAW_LEN: usize = 58_064;
static PACKED: &[u8] = include_bytes!("vlc_tables_packed.bin");

pub struct Rv34RawVlcLengths {
    pub intra_cbppat: [[[u8; CBPPAT_VLC_SIZE]; 2]; NUM_INTRA_TABLES],
    pub intra_cbp: [[[u8; CBP_VLC_SIZE]; 8]; NUM_INTRA_TABLES],
    pub intra_firstpat: [[[u8; FIRSTBLK_VLC_SIZE]; 4]; NUM_INTRA_TABLES],
    pub intra_secondpat: [[[u8; OTHERBLK_VLC_SIZE]; 2]; NUM_INTRA_TABLES],
    pub intra_thirdpat: [[[u8; OTHERBLK_VLC_SIZE]; 2]; NUM_INTRA_TABLES],
    pub intra_coeff: [[u8; COEFF_VLC_SIZE]; NUM_INTRA_TABLES],
    pub inter_cbppat: [[u8; CBPPAT_VLC_SIZE]; NUM_INTER_TABLES],
    pub inter_cbp: [[[u8; CBP_VLC_SIZE]; 4]; NUM_INTER_TABLES],
    pub inter_firstpat: [[[u8; FIRSTBLK_VLC_SIZE]; 2]; NUM_INTER_TABLES],
    pub inter_secondpat: [[[u8; OTHERBLK_VLC_SIZE]; 2]; NUM_INTER_TABLES],
    pub inter_thirdpat: [[[u8; OTHERBLK_VLC_SIZE]; 2]; NUM_INTER_TABLES],
    pub inter_coeff: [[u8; COEFF_VLC_SIZE]; NUM_INTER_TABLES],
}

// Expands RV34 VLC code-lengths once during LazyLock table build, then frees all scratch memory.
pub fn unpack_rv34_vlc_lengths() -> Box<Rv34RawVlcLengths> {
    let mut raw = Vec::with_capacity(RAW_LEN);
    let mut i = 0usize;
    while raw.len() < RAW_LEN && i < PACKED.len() {
        let flags = PACKED[i];
        i += 1;
        for bit in 0..8 {
            if raw.len() >= RAW_LEN || i >= PACKED.len() {
                break;
            }
            if (flags & (1 << bit)) != 0 {
                let enc = (u16::from(PACKED[i]) << 8) | u16::from(PACKED[i + 1]);
                i += 2;
                let off = usize::from(enc >> 4) + 1;
                let len = usize::from(enc & 0x0f) + 3;
                let start = raw.len() - off;
                for k in 0..len {
                    let b = raw[start + k];
                    raw.push(b);
                }
            } else {
                raw.push(PACKED[i]);
                i += 1;
            }
        }
    }
    debug_assert_eq!(raw.len(), RAW_LEN);
    let mut pos = 0usize;
    let mut take = |dst: &mut [u8]| {
        dst.copy_from_slice(&raw[pos..pos + dst.len()]);
        pos += dst.len();
    };
    let mut out = Box::new(Rv34RawVlcLengths {
        intra_cbppat: [[[0; CBPPAT_VLC_SIZE]; 2]; NUM_INTRA_TABLES],
        intra_cbp: [[[0; CBP_VLC_SIZE]; 8]; NUM_INTRA_TABLES],
        intra_firstpat: [[[0; FIRSTBLK_VLC_SIZE]; 4]; NUM_INTRA_TABLES],
        intra_secondpat: [[[0; OTHERBLK_VLC_SIZE]; 2]; NUM_INTRA_TABLES],
        intra_thirdpat: [[[0; OTHERBLK_VLC_SIZE]; 2]; NUM_INTRA_TABLES],
        intra_coeff: [[0; COEFF_VLC_SIZE]; NUM_INTRA_TABLES],
        inter_cbppat: [[0; CBPPAT_VLC_SIZE]; NUM_INTER_TABLES],
        inter_cbp: [[[0; CBP_VLC_SIZE]; 4]; NUM_INTER_TABLES],
        inter_firstpat: [[[0; FIRSTBLK_VLC_SIZE]; 2]; NUM_INTER_TABLES],
        inter_secondpat: [[[0; OTHERBLK_VLC_SIZE]; 2]; NUM_INTER_TABLES],
        inter_thirdpat: [[[0; OTHERBLK_VLC_SIZE]; 2]; NUM_INTER_TABLES],
        inter_coeff: [[0; COEFF_VLC_SIZE]; NUM_INTER_TABLES],
    });
    for a in &mut out.intra_cbppat {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.intra_cbp {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.intra_firstpat {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.intra_secondpat {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.intra_thirdpat {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.intra_coeff {
        take(a);
    }
    for a in &mut out.inter_cbppat {
        take(a);
    }
    for a in &mut out.inter_cbp {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.inter_firstpat {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.inter_secondpat {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.inter_thirdpat {
        for b in a {
            take(b);
        }
    }
    for a in &mut out.inter_coeff {
        take(a);
    }
    debug_assert_eq!(pos, RAW_LEN);
    out
}
