// MACE (Macintosh Audio Compression/Expansion) 3:1 and 6:1 decoder.
//
// Ported from FFmpeg libavcodec/mace.c (commit 2da55bf), LGPL-2.1-or-later.

//! MACE 3:1 and 6:1: the `MACEtab1..4` tables, the index/factor state
//! machine (`read_table`, `chomp3`, `chomp6`), the asymmetric
//! `mace_broken_clip_int16` that keeps the output identical to Apple's
//! decoder, and the `QT_8S_2_16S` byte duplication. Output is S16P, 3
//! (MACE 3:1) or 6 (MACE 6:1) samples per input byte across the channels,
//! bit-exact with FFmpeg. The channel state carries over from packet to
//! packet.

#![allow(clippy::needless_range_loop)]

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

const MACE_TAB1: [i16; 8] = [-13, 8, 76, 222, 222, 76, 8, -13];
const MACE_TAB3: [i16; 4] = [-18, 140, 140, -18];

#[rustfmt::skip]
const MACE_TAB2: [[i16; 4]; 128] = [
    [   37,   116,   206,   330], [   39,   121,   216,   346],
    [   41,   127,   225,   361], [   42,   132,   235,   377],
    [   44,   137,   245,   392], [   46,   144,   256,   410],
    [   48,   150,   267,   428], [   51,   157,   280,   449],
    [   53,   165,   293,   470], [   55,   172,   306,   490],
    [   58,   179,   319,   511], [   60,   187,   333,   534],
    [   63,   195,   348,   557], [   66,   205,   364,   583],
    [   69,   214,   380,   609], [   72,   223,   396,   635],
    [   75,   233,   414,   663], [   79,   244,   433,   694],
    [   82,   254,   453,   725], [   86,   265,   472,   756],
    [   90,   278,   495,   792], [   94,   290,   516,   826],
    [   98,   303,   538,   862], [  102,   316,   562,   901],
    [  107,   331,   588,   942], [  112,   345,   614,   983],
    [  117,   361,   641,  1027], [  122,   377,   670,  1074],
    [  127,   394,   701,  1123], [  133,   411,   732,  1172],
    [  139,   430,   764,  1224], [  145,   449,   799,  1280],
    [  152,   469,   835,  1337], [  159,   490,   872,  1397],
    [  166,   512,   911,  1459], [  173,   535,   951,  1523],
    [  181,   558,   993,  1590], [  189,   584,  1038,  1663],
    [  197,   610,  1085,  1738], [  206,   637,  1133,  1815],
    [  215,   665,  1183,  1895], [  225,   695,  1237,  1980],
    [  235,   726,  1291,  2068], [  246,   759,  1349,  2161],
    [  257,   792,  1409,  2257], [  268,   828,  1472,  2357],
    [  280,   865,  1538,  2463], [  293,   903,  1606,  2572],
    [  306,   944,  1678,  2688], [  319,   986,  1753,  2807],
    [  334,  1030,  1832,  2933], [  349,  1076,  1914,  3065],
    [  364,  1124,  1999,  3202], [  380,  1174,  2088,  3344],
    [  398,  1227,  2182,  3494], [  415,  1281,  2278,  3649],
    [  434,  1339,  2380,  3811], [  453,  1398,  2486,  3982],
    [  473,  1461,  2598,  4160], [  495,  1526,  2714,  4346],
    [  517,  1594,  2835,  4540], [  540,  1665,  2961,  4741],
    [  564,  1740,  3093,  4953], [  589,  1818,  3232,  5175],
    [  615,  1898,  3375,  5405], [  643,  1984,  3527,  5647],
    [  671,  2072,  3683,  5898], [  701,  2164,  3848,  6161],
    [  733,  2261,  4020,  6438], [  766,  2362,  4199,  6724],
    [  800,  2467,  4386,  7024], [  836,  2578,  4583,  7339],
    [  873,  2692,  4786,  7664], [  912,  2813,  5001,  8008],
    [  952,  2938,  5223,  8364], [  995,  3070,  5457,  8739],
    [ 1039,  3207,  5701,  9129], [ 1086,  3350,  5956,  9537],
    [ 1134,  3499,  6220,  9960], [ 1185,  3655,  6497, 10404],
    [ 1238,  3818,  6788, 10869], [ 1293,  3989,  7091, 11355],
    [ 1351,  4166,  7407, 11861], [ 1411,  4352,  7738, 12390],
    [ 1474,  4547,  8084, 12946], [ 1540,  4750,  8444, 13522],
    [ 1609,  4962,  8821, 14126], [ 1680,  5183,  9215, 14756],
    [ 1756,  5415,  9626, 15415], [ 1834,  5657, 10057, 16104],
    [ 1916,  5909, 10505, 16822], [ 2001,  6173, 10975, 17574],
    [ 2091,  6448, 11463, 18356], [ 2184,  6736, 11974, 19175],
    [ 2282,  7037, 12510, 20032], [ 2383,  7351, 13068, 20926],
    [ 2490,  7679, 13652, 21861], [ 2601,  8021, 14260, 22834],
    [ 2717,  8380, 14897, 23854], [ 2838,  8753, 15561, 24918],
    [ 2965,  9144, 16256, 26031], [ 3097,  9553, 16982, 27193],
    [ 3236,  9979, 17740, 28407], [ 3380, 10424, 18532, 29675],
    [ 3531, 10890, 19359, 31000], [ 3688, 11375, 20222, 32382],
    [ 3853, 11883, 21125, 32767], [ 4025, 12414, 22069, 32767],
    [ 4205, 12967, 23053, 32767], [ 4392, 13546, 24082, 32767],
    [ 4589, 14151, 25157, 32767], [ 4793, 14783, 26280, 32767],
    [ 5007, 15442, 27452, 32767], [ 5231, 16132, 28678, 32767],
    [ 5464, 16851, 29957, 32767], [ 5708, 17603, 31294, 32767],
    [ 5963, 18389, 32691, 32767], [ 6229, 19210, 32767, 32767],
    [ 6507, 20067, 32767, 32767], [ 6797, 20963, 32767, 32767],
    [ 7101, 21899, 32767, 32767], [ 7418, 22876, 32767, 32767],
    [ 7749, 23897, 32767, 32767], [ 8095, 24964, 32767, 32767],
    [ 8456, 26078, 32767, 32767], [ 8833, 27242, 32767, 32767],
    [ 9228, 28457, 32767, 32767], [ 9639, 29727, 32767, 32767],
];

#[rustfmt::skip]
const MACE_TAB4: [[i16; 2]; 128] = [
    [   64,   216], [   67,   226], [   70,   236], [   74,   246],
    [   77,   257], [   80,   268], [   84,   280], [   88,   294],
    [   92,   307], [   96,   321], [  100,   334], [  104,   350],
    [  109,   365], [  114,   382], [  119,   399], [  124,   416],
    [  130,   434], [  136,   454], [  142,   475], [  148,   495],
    [  155,   519], [  162,   541], [  169,   564], [  176,   590],
    [  185,   617], [  193,   644], [  201,   673], [  210,   703],
    [  220,   735], [  230,   767], [  240,   801], [  251,   838],
    [  262,   876], [  274,   914], [  286,   955], [  299,   997],
    [  312,  1041], [  326,  1089], [  341,  1138], [  356,  1188],
    [  372,  1241], [  388,  1297], [  406,  1354], [  424,  1415],
    [  443,  1478], [  462,  1544], [  483,  1613], [  505,  1684],
    [  527,  1760], [  551,  1838], [  576,  1921], [  601,  2007],
    [  628,  2097], [  656,  2190], [  686,  2288], [  716,  2389],
    [  748,  2496], [  781,  2607], [  816,  2724], [  853,  2846],
    [  891,  2973], [  930,  3104], [  972,  3243], [ 1016,  3389],
    [ 1061,  3539], [ 1108,  3698], [ 1158,  3862], [ 1209,  4035],
    [ 1264,  4216], [ 1320,  4403], [ 1379,  4599], [ 1441,  4806],
    [ 1505,  5019], [ 1572,  5244], [ 1642,  5477], [ 1715,  5722],
    [ 1792,  5978], [ 1872,  6245], [ 1955,  6522], [ 2043,  6813],
    [ 2134,  7118], [ 2229,  7436], [ 2329,  7767], [ 2432,  8114],
    [ 2541,  8477], [ 2655,  8854], [ 2773,  9250], [ 2897,  9663],
    [ 3026, 10094], [ 3162, 10546], [ 3303, 11016], [ 3450, 11508],
    [ 3604, 12020], [ 3765, 12556], [ 3933, 13118], [ 4108, 13703],
    [ 4292, 14315], [ 4483, 14953], [ 4683, 15621], [ 4892, 16318],
    [ 5111, 17046], [ 5339, 17807], [ 5577, 18602], [ 5826, 19433],
    [ 6086, 20300], [ 6358, 21205], [ 6642, 22152], [ 6938, 23141],
    [ 7248, 24173], [ 7571, 25252], [ 7909, 26380], [ 8262, 27557],
    [ 8631, 28786], [ 9016, 30072], [ 9419, 31413], [ 9839, 32767],
    [10278, 32767], [10737, 32767], [11216, 32767], [11717, 32767],
    [12240, 32767], [12786, 32767], [13356, 32767], [13953, 32767],
    [14576, 32767], [15226, 32767], [15906, 32767], [16615, 32767],
];

/// FFmpeg's `tabs[]`: per `tab_idx`, the base table, the 2-D stride
/// table flattened as `[row][stride]`, and the stride.
const TABS: [(&[i16], &[i16], usize); 3] = [
    (&MACE_TAB1, MACE_TAB2.as_flattened(), 4),
    (&MACE_TAB3, MACE_TAB4.as_flattened(), 2),
    (&MACE_TAB1, MACE_TAB2.as_flattened(), 4),
];

/// MACE version of `av_clip_int16` (asymmetric: -32767 not -32768) to
/// keep binary-identical output to the reference decoder.
#[inline]
fn mace_broken_clip_int16(n: i32) -> i16 {
    if n > 32767 {
        32767
    } else if n < -32768 {
        -32767
    } else {
        n as i16
    }
}

/// `QT_8S_2_16S`: swap the bytes of a 16-bit value produced by the
/// (big-endian) tables.
#[inline]
fn qt_8s_2_16s(x: i16) -> i16 {
    let u = x as u16;
    (((u & 0xFF00) | ((u >> 8) & 0xFF)) as i16 as u16).wrapping_mul(1) as i16
}

#[derive(Clone, Copy, Default)]
struct ChannelData {
    index: i16,
    factor: i16,
    prev2: i16,
    previous: i16,
    level: i16,
}

/// FFmpeg `read_table`.
fn read_table(chd: &mut ChannelData, val: u8, tab_idx: usize) -> i16 {
    let (tab1, tab2, stride) = TABS[tab_idx];
    let idx_row = ((chd.index as i32 & 0x7f0) >> 4) as usize;
    let current: i16 = if (val as usize) < stride {
        tab2[idx_row * stride + val as usize]
    } else {
        -1 - tab2[idx_row * stride + 2 * stride - val as usize - 1]
    };

    let next = chd
        .index
        .wrapping_add(tab1[val as usize].wrapping_sub(chd.index >> 5));
    chd.index = if next < 0 { 0 } else { next };

    current
}

/// FFmpeg `chomp3`.
fn chomp3(chd: &mut ChannelData, val: u8, tab_idx: usize) -> i16 {
    let current = read_table(chd, val, tab_idx);
    let current = mace_broken_clip_int16(current as i32 + chd.level as i32);
    chd.level = current.wrapping_sub(current >> 3);
    qt_8s_2_16s(current)
}

/// FFmpeg `chomp6`: produces the two interlaced output samples.
fn chomp6(chd: &mut ChannelData, val: u8, tab_idx: usize) -> (i16, i16) {
    let current = read_table(chd, val, tab_idx);

    if (chd.previous ^ current) >= 0 {
        chd.factor = (chd.factor as i32 + 506).min(32767) as i16;
    } else if chd.factor as i32 - 314 < -32768 {
        chd.factor = -32767;
    } else {
        chd.factor = chd.factor.wrapping_sub(314);
    }

    let current = mace_broken_clip_int16(current as i32 + chd.level as i32);
    chd.level = ((current as i32 * chd.factor as i32) >> 15) as i16;
    let current = current >> 1;

    let out0 = qt_8s_2_16s(
        chd
            .previous
            .wrapping_add(chd.prev2)
            .wrapping_sub((chd.prev2.wrapping_sub(current)) >> 2),
    );
    let out1 = qt_8s_2_16s(
        chd
            .previous
            .wrapping_add(current)
            .wrapping_add((chd.prev2.wrapping_sub(current)) >> 2),
    );
    chd.prev2 = chd.previous;
    chd.previous = current;
    (out0, out1)
}

/// FFmpeg `mace_decode_frame`.
fn mace_decode_frame_data(
    chd: &mut [ChannelData; 2],
    buf: &[u8],
    channels: usize,
    is_mace3: bool,
) -> Result<(usize, Vec<Vec<u8>>)> {
    let mut buf_size = buf.len();
    let unit = channels << (is_mace3 as usize);
    if buf_size % unit != 0 {
        buf_size -= buf_size % unit;
        if buf_size == 0 {
            return Err(Error::invalid("mace: buffer size is odd"));
        }
    }

    // nb_samples = 3 * (buf_size << (1 - is_mace3)) / channels.
    let nb_samples = 3 * (buf_size << (1 - is_mace3 as usize)) / channels;
    if nb_samples > i32::MAX as usize {
        return Err(Error::invalid("mace: frame too large"));
    }

    let mut out: Vec<Vec<i16>> = vec![Vec::with_capacity(nb_samples); channels];

    for i in 0..channels {
        for j in 0..buf_size / unit {
            for k in 0..(1 << is_mace3 as usize) {
                // FFmpeg: buf[(i << is_mace3) + (j * channels << is_mace3) + k]
                // C precedence: (j * channels) << is_mace3.
                let byte_idx =
                    (i << (is_mace3 as usize)) + ((j * channels) << (is_mace3 as usize)) + k;
                let pkt = buf[byte_idx];

                let val = [
                    [pkt >> 5, (pkt >> 3) & 3, pkt & 7],
                    [pkt & 7, (pkt >> 3) & 3, pkt >> 5],
                ];

                for l in 0..3 {
                    if is_mace3 {
                        // MACE3: chomp3 writes one sample; the output
                        // pointer advances by 1 << (1 - is_mace3) = 1.
                        let s = chomp3(&mut chd[i], val[1][l], l);
                        out[i].push(s);
                    } else {
                        // MACE6: chomp6 writes two interlaced samples
                        // (output[0], output[1]); the pointer advances
                        // by 1 << (1 - is_mace3) = 2.
                        let (s0, s1) = chomp6(&mut chd[i], val[0][l], l);
                        out[i].push(s0);
                        out[i].push(s1);
                    }
                }
            }
        }
    }

    let mut planes = Vec::with_capacity(channels);
    for ch_plane in out.iter_mut().take(channels) {
        ch_plane.truncate(nb_samples);
        let mut bytes = Vec::with_capacity(nb_samples * 2);
        for s in ch_plane.iter() {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        planes.push(bytes);
    }

    Ok((nb_samples, planes))
}

/// `mace_decode_init`: one or two channels, as the container declares;
/// S16P out.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let channels = match params.channels {
        Some(c @ 1..=2) => usize::from(c),
        other => return Err(Error::invalid(format!("mace: {other:?} channels"))),
    };
    Ok(Box::new(MaceDecoder {
        codec_id: params.codec_id.clone(),
        is_mace3: params.codec_id.as_str() == "mace3",
        channels,
        sample_rate: params.sample_rate.unwrap_or(0),
        chd: [ChannelData::default(); 2],
        out: VecDeque::new(),
    }))
}

struct MaceDecoder {
    codec_id: CodecId,
    is_mace3: bool,
    channels: usize,
    sample_rate: u32,
    chd: [ChannelData; 2],
    out: VecDeque<Frame>,
}

impl Decoder for MaceDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let (nb_samples, planes) =
            mace_decode_frame_data(&mut self.chd, &packet.data, self.channels, self.is_mace3)?;
        self.out.push_back(Frame::Audio(AudioFrame { samples: nb_samples as u32, pts: packet.pts, data: planes }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.out.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.out.clear();
        self.chd = [ChannelData::default(); 2];
        Ok(())
    }

    /// MACE carries no rate of its own: the container's.
    fn output_audio_format(&self) -> Option<AudioFormat> {
        (self.sample_rate > 0).then(|| AudioFormat {
            sample_format: SampleFormat::S16P,
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}
