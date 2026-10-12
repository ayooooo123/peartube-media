// Ported from FFmpeg (commit 2da55bf): libavcodec/dv_profile.c (dv_profiles,
// ff_dv_frame_profile), dv_profile.h (AVDVProfile, DV_PROFILE_BYTES), dv.h
// (DV_PROFILE_IS_*), dv.c (dv_calc_mb_coordinates,
// ff_dv_init_dynamic_tables) and dv_internal.h (dv_work_pool_size,
// dv_calculate_mb_xy).
// License: LGPL-2.1-or-later

//! The DV profiles (IEC 61834, SMPTE 314M DV25/DV50, SMPTE 370M DVCPRO HD),
//! how a frame names its profile, and where each macroblock of a video
//! segment goes in the picture.

use oxideav_core::PixelFormat;

use crate::tables::{
    BLOCK_SIZES_DV100, BLOCK_SIZES_DV2550, DV_AUDIO_SHUFFLE525, DV_AUDIO_SHUFFLE625, MB_L_START, MB_L_START_SHUFFLED, MB_OFF,
    MB_REMAP, MB_SERPENT1, MB_SERPENT2, MB_SHUF1, MB_SHUF2, MB_SHUF3,
};

/// DV_PROFILE_BYTES: the six DIF blocks a profile is read from.
pub const DV_PROFILE_BYTES: usize = 6 * 80;
/// DV_MAX_FRAME_SIZE.
pub const DV_MAX_FRAME_SIZE: usize = 576_000;
/// DV_MAX_BPM.
pub const DV_MAX_BPM: usize = 8;

/// AVDVProfile, without what only FFmpeg's muxer, timecodes and aspect
/// ratios use (ltc_divisor, sar, audio_samples_dist).
#[derive(Debug)]
pub struct DvProfile {
    /// The dsf flag of the DIF header.
    pub dsf: u8,
    /// The stype of the VAUX source pack.
    pub video_stype: u8,
    /// Bytes of one frame.
    pub frame_size: usize,
    /// DIF segments per DIF channel.
    pub difseg_size: usize,
    /// DIF channels per frame.
    pub n_difchan: usize,
    /// 1/framerate, (num, den).
    pub time_base: (i64, i64),
    pub height: usize,
    pub width: usize,
    pub pix_fmt: PixelFormat,
    /// Blocks per macroblock.
    pub bpm: usize,
    /// AC bit budget of each block.
    pub block_sizes: &'static [u8; 8],
    pub audio_stride: usize,
    /// Fewest audio samples a frame has at 48, 44.1 and 32 kHz.
    pub audio_min_samples: [usize; 3],
    /// PCM shuffling.
    pub audio_shuffle: &'static [[u8; 9]],
}

impl DvProfile {
    /// DV_PROFILE_IS_HD.
    pub fn is_hd(&self) -> bool {
        self.video_stype & 0x10 != 0
    }

    /// DV_PROFILE_IS_1080i50.
    pub fn is_1080i50(&self) -> bool {
        self.video_stype == 0x14 && self.dsf == 1
    }

    /// DV_PROFILE_IS_720p50.
    pub fn is_720p50(&self) -> bool {
        self.video_stype == 0x18 && self.dsf == 1
    }

    /// dv_work_pool_size: the video segments of a frame.
    pub fn work_pool_size(&self) -> usize {
        let mut size = self.n_difchan * self.difseg_size * 27;
        if self.is_1080i50() {
            size -= 3 * 27;
        }
        if self.is_720p50() {
            size -= 4 * 27;
        }
        size
    }
}

const NTSC_SAMPLES: [usize; 3] = [1580, 1452, 1053];
const PAL_SAMPLES: [usize; 3] = [1896, 1742, 1264];

/// dv_profiles.
pub static DV_PROFILES: [DvProfile; 10] = [
    // IEC 61834, SMPTE-314M - 525/60 (NTSC)
    DvProfile {
        dsf: 0,
        video_stype: 0x0,
        frame_size: 120_000,
        difseg_size: 10,
        n_difchan: 1,
        time_base: (1001, 30000),
        height: 480,
        width: 720,
        pix_fmt: PixelFormat::Yuv411P,
        bpm: 6,
        block_sizes: &BLOCK_SIZES_DV2550,
        audio_stride: 90,
        audio_min_samples: NTSC_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE525,
    },
    // IEC 61834 - 625/50 (PAL)
    DvProfile {
        dsf: 1,
        video_stype: 0x0,
        frame_size: 144_000,
        difseg_size: 12,
        n_difchan: 1,
        time_base: (1, 25),
        height: 576,
        width: 720,
        pix_fmt: PixelFormat::Yuv420P,
        bpm: 6,
        block_sizes: &BLOCK_SIZES_DV2550,
        audio_stride: 108,
        audio_min_samples: PAL_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE625,
    },
    // SMPTE-314M - 625/50 (PAL)
    DvProfile {
        dsf: 1,
        video_stype: 0x0,
        frame_size: 144_000,
        difseg_size: 12,
        n_difchan: 1,
        time_base: (1, 25),
        height: 576,
        width: 720,
        pix_fmt: PixelFormat::Yuv411P,
        bpm: 6,
        block_sizes: &BLOCK_SIZES_DV2550,
        audio_stride: 108,
        audio_min_samples: PAL_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE625,
    },
    // SMPTE-314M - 525/60 (NTSC) 50 Mbps, "DVCPRO50"
    DvProfile {
        dsf: 0,
        video_stype: 0x4,
        frame_size: 240_000,
        difseg_size: 10,
        n_difchan: 2,
        time_base: (1001, 30000),
        height: 480,
        width: 720,
        pix_fmt: PixelFormat::Yuv422P,
        bpm: 6,
        block_sizes: &BLOCK_SIZES_DV2550,
        audio_stride: 90,
        audio_min_samples: NTSC_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE525,
    },
    // SMPTE-314M - 625/50 (PAL) 50 Mbps, "DVCPRO50"
    DvProfile {
        dsf: 1,
        video_stype: 0x4,
        frame_size: 288_000,
        difseg_size: 12,
        n_difchan: 2,
        time_base: (1, 25),
        height: 576,
        width: 720,
        pix_fmt: PixelFormat::Yuv422P,
        bpm: 6,
        block_sizes: &BLOCK_SIZES_DV2550,
        audio_stride: 108,
        audio_min_samples: PAL_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE625,
    },
    // SMPTE-370M - 1080i60 100 Mbps, "DVCPRO HD"
    DvProfile {
        dsf: 0,
        video_stype: 0x14,
        frame_size: 480_000,
        difseg_size: 10,
        n_difchan: 4,
        time_base: (1001, 30000),
        height: 1080,
        width: 1280,
        pix_fmt: PixelFormat::Yuv422P,
        bpm: 8,
        block_sizes: &BLOCK_SIZES_DV100,
        audio_stride: 90,
        audio_min_samples: NTSC_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE525,
    },
    // SMPTE-370M - 1080i50 100 Mbps, "DVCPRO HD"
    DvProfile {
        dsf: 1,
        video_stype: 0x14,
        frame_size: 576_000,
        difseg_size: 12,
        n_difchan: 4,
        time_base: (1, 25),
        height: 1080,
        width: 1440,
        pix_fmt: PixelFormat::Yuv422P,
        bpm: 8,
        block_sizes: &BLOCK_SIZES_DV100,
        audio_stride: 108,
        audio_min_samples: PAL_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE625,
    },
    // SMPTE-370M - 720p60 100 Mbps, "DVCPRO HD"
    DvProfile {
        dsf: 0,
        video_stype: 0x18,
        frame_size: 240_000,
        difseg_size: 10,
        n_difchan: 2,
        time_base: (1001, 60000),
        height: 720,
        width: 960,
        pix_fmt: PixelFormat::Yuv422P,
        bpm: 8,
        block_sizes: &BLOCK_SIZES_DV100,
        audio_stride: 90,
        audio_min_samples: NTSC_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE525,
    },
    // SMPTE-370M - 720p50 100 Mbps, "DVCPRO HD"
    DvProfile {
        dsf: 1,
        video_stype: 0x18,
        frame_size: 288_000,
        difseg_size: 12,
        n_difchan: 2,
        time_base: (1, 50),
        height: 720,
        width: 960,
        pix_fmt: PixelFormat::Yuv422P,
        bpm: 8,
        block_sizes: &BLOCK_SIZES_DV100,
        audio_stride: 90,
        audio_min_samples: PAL_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE625,
    },
    // IEC 61883-5 - 625/50 (PAL)
    DvProfile {
        dsf: 1,
        video_stype: 0x1,
        frame_size: 144_000,
        difseg_size: 12,
        n_difchan: 1,
        time_base: (1, 25),
        height: 576,
        width: 720,
        pix_fmt: PixelFormat::Yuv420P,
        bpm: 6,
        block_sizes: &BLOCK_SIZES_DV2550,
        audio_stride: 108,
        audio_min_samples: PAL_SAMPLES,
        audio_shuffle: &DV_AUDIO_SHUFFLE625,
    },
];

/// ff_dv_frame_profile: the profile `frame` (the first `buf_size` bytes
/// of the input) names. `sl25_576` is the codec context check for 4:1:1
/// PAL in "SL25" (codec_tag SL25, coded size 720x576); `sys` the profile
/// of the previous frame.
pub fn frame_profile(sys: Option<&'static DvProfile>, frame: &[u8], buf_size: usize, sl25_576: bool) -> Option<&'static DvProfile> {
    if buf_size < DV_PROFILE_BYTES || frame.len() < DV_PROFILE_BYTES {
        return None;
    }
    let dsf = (frame[3] & 0x80) >> 7;
    let vs = frame[80 * 5 + 48 + 3];
    let stype = vs & 0x1f;
    let pal = vs & 0x20 != 0;
    // 576i50 25Mbps 4:1:1 is a special case
    if (dsf == 1 && stype == 0 && frame[4] & 0x07 != 0) || (stype == 31 && sl25_576) {
        return Some(&DV_PROFILES[2]);
    }
    // PAL DV files with dsf flag 0 (trac #8333, #2177): by the pal flag and
    // buf_size.
    if dsf == 0 && pal && stype == DV_PROFILES[1].video_stype && buf_size == DV_PROFILES[1].frame_size {
        return Some(&DV_PROFILES[1]);
    }
    if let Some(p) = DV_PROFILES.iter().find(|p| p.dsf == dsf && p.video_stype == stype) {
        return Some(p);
    }
    // The old profile, for corrupted input.
    if let Some(sys) = sys.filter(|s| buf_size == s.frame_size) {
        return Some(sys);
    }
    // QuickTime 3 files (trac #217).
    if (frame[3] & 0x7f) == 0x3f && vs == 0xff {
        return Some(&DV_PROFILES[usize::from(dsf)]);
    }
    None
}

/// DVwork_chunk: a video segment's first DIF block (in 80-byte units) and
/// where its five macroblocks go.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkChunk {
    pub buf_offset: u16,
    pub mb_coordinates: [u16; 5],
}

/// dv_calc_mb_coordinates.
fn calc_mb_coordinates(d: &DvProfile, chan: usize, seq: usize, slot: usize, tbl: &mut [u16; 5]) {
    let chan = chan as i32;
    let seq = seq as i32;
    let slot = slot as i32;
    let difseg = d.difseg_size as i32;
    for (m, entry) in tbl.iter_mut().enumerate() {
        let off = i32::from(MB_OFF[m]);
        let value = match d.width {
            1440 => {
                let blk = (chan * 11 + seq) * 27 + slot;
                let (x, y) = if chan == 0 && seq == 11 {
                    let x = m as i32 * 27 + slot;
                    if x < 90 {
                        (x, 0)
                    } else {
                        ((x - 90) * 2, 67)
                    }
                } else {
                    let i = (4 * chan + blk + off) % 11;
                    let k = (blk / 11) % 27;
                    (i32::from(MB_SHUF1[m]) + (chan & 1) * 9 + k % 9, (i * 3 + k / 9) * 2 + (chan >> 1) + 1)
                };
                (x << 1) | (y << 9)
            }
            1280 => {
                let blk = (chan * 10 + seq) * 27 + slot;
                let i = (4 * chan + (seq / 5) + 2 * blk + off) % 10;
                let k = (blk / 5) % 27;
                let mut x = i32::from(MB_SHUF1[m]) + (chan & 1) * 9 + k % 9;
                let mut y = (i * 3 + k / 9) * 2 + (chan >> 1) + 4;
                if x >= 80 {
                    let r = MB_REMAP[y as usize];
                    x = i32::from(r[0]) + ((x - 80) << i32::from(y > 59));
                    y = i32::from(r[1]);
                }
                (x << 1) | (y << 9)
            }
            960 => {
                let blk = (chan * 10 + seq) * 27 + slot;
                let i = (4 * chan + (seq / 5) + 2 * blk + off) % 10;
                let k = (blk / 5) % 27 + (i & 1) * 3;
                let x = i32::from(MB_SHUF2[m]) + k % 6 + 6 * (chan & 1);
                let y = i32::from(MB_L_START[i as usize]) + k / 6 + 45 * (chan >> 1);
                (x << 1) | (y << 9)
            }
            720 => match d.pix_fmt {
                PixelFormat::Yuv422P => {
                    let x = i32::from(MB_SHUF3[m]) + slot / 3;
                    let y = i32::from(MB_SERPENT1[slot as usize]) + ((((seq + off) % difseg) << 1) + chan) * 3;
                    (x << 1) | (y << 8)
                }
                PixelFormat::Yuv420P => {
                    let x = i32::from(MB_SHUF3[m]) + slot / 3;
                    let y = i32::from(MB_SERPENT1[slot as usize]) + ((seq + off) % difseg) * 3;
                    (x << 1) | (y << 9)
                }
                PixelFormat::Yuv411P => {
                    let i = (seq + off) % difseg;
                    let k = slot + if m == 1 || m == 2 { 3 } else { 0 };
                    let x = i32::from(MB_L_START_SHUFFLED[m]) + k / 6;
                    let mut y = i32::from(MB_SERPENT2[k as usize]) + i * 6;
                    if x > 21 {
                        y = y * 2 - i * 6;
                    }
                    (x << 2) | (y << 8)
                }
                _ => continue,
            },
            _ => continue,
        };
        *entry = value as u16;
    }
}

/// ff_dv_init_dynamic_tables: the video segments of a frame of profile
/// `d`, in decoding order.
pub fn work_chunks(d: &DvProfile) -> Vec<WorkChunk> {
    let mut chunks = Vec::with_capacity(d.work_pool_size());
    let mut p = 0usize;
    for c in 0..d.n_difchan {
        for s in 0..d.difseg_size {
            p += 6;
            for j in 0..27 {
                p += usize::from(j % 3 == 0);
                if !(d.is_1080i50() && c != 0 && s == 11) && !(d.is_720p50() && s > 9) {
                    let mut chunk = WorkChunk { buf_offset: p as u16, ..WorkChunk::default() };
                    calc_mb_coordinates(d, c, s, j, &mut chunk.mb_coordinates);
                    chunks.push(chunk);
                }
                p += 5;
            }
        }
    }
    chunks
}

/// dv_calculate_mb_xy: macroblock `m` of `chunk` in 8-pixel units. 720p
/// frames come in halves; the second (DIF channels 2 and 3) is displaced.
pub fn mb_xy(sys: &DvProfile, buf1: u8, chunk: &WorkChunk, m: usize) -> (usize, usize) {
    let mb_x = usize::from(chunk.mb_coordinates[m] & 0xff);
    let mut mb_y = i32::from(chunk.mb_coordinates[m] >> 8);
    if sys.height == 720 && buf1 & 0x0C == 0 {
        // shifting the Y coordinate down by 72/2 macro blocks
        mb_y -= if mb_y > 17 { 18 } else { -72 };
    }
    (mb_x, mb_y.max(0) as usize)
}
