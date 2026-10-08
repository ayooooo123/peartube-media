//! Regression tests for P1 reviewer findings on codec-ape.
//! These tests verify that overflow panics and divergent behavior on crafted
//! inputs are prevented in debug builds.

use std::io::Cursor;
use oxideav_core::{CodecParameters, Packet, RuntimeContext, TimeBase};

#[test]
fn test_p1_update_rice_overflow() {
    let mut rice = codec_ape::entropy::APERice { k: 10, ksum: 100 };
    // x = u32::MAX: in debug build without wrapping_add(1), (x + 1) overflows
    codec_ape::entropy::update_rice(&mut rice, u32::MAX);

    // rice.ksum near u32::MAX: without wrapping_add(16), (ksum + 16) overflows
    rice.ksum = 0xFFFF_FFF5;
    codec_ape::entropy::update_rice(&mut rice, 100);
}

#[test]
fn test_p1_ape_decode_value_3860_ksum_overflow() {
    let mut rice = codec_ape::entropy::APERice { k: 10, ksum: 0xFFFF_FFF8 };
    let mut error = false;
    let data = [0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00];
    let mut gb = codec_ape::entropy::BitReader::new(&data);
    // In debug build without wrapping_add(8), (rice.ksum + 8) overflows
    let _ = codec_ape::entropy::ape_decode_value_3860(3880, &mut gb, &mut rice, &mut error);
}

#[test]
fn test_p1_demuxer_seek_table_skip_underflow() {
    // Craft a valid APE header with 2 frames where seektable[1] < firstframe.
    // In debug build without wrapping_sub, (pos - first_pos) & 3 panics.
    let mut data = Vec::new();
    data.extend_from_slice(b"MAC ");
    data.extend_from_slice(&3800u16.to_le_bytes()); // version
    data.extend_from_slice(&2000u16.to_le_bytes()); // compressiontype
    data.extend_from_slice(&0u16.to_le_bytes());    // formatflags
    data.extend_from_slice(&2u16.to_le_bytes());    // channels
    data.extend_from_slice(&44100u32.to_le_bytes()); // samplerate
    data.extend_from_slice(&0u32.to_le_bytes());    // wavheaderlength
    data.extend_from_slice(&0u32.to_le_bytes());    // wavtaillength
    data.extend_from_slice(&2u32.to_le_bytes());    // totalframes = 2
    data.extend_from_slice(&1000u32.to_le_bytes()); // finalframeblocks

    // Seektable: totalframes * 4 = 8 bytes.
    // seektable[0] (skipped)
    data.extend_from_slice(&100u32.to_le_bytes());
    // seektable[1] = 0, so frames[1].pos (0) < firstframe (~46)
    data.extend_from_slice(&0u32.to_le_bytes());

    // Padding / frames payload
    data.extend_from_slice(&[0u8; 128]);

    let ctx = RuntimeContext::new();
    let cursor = Box::new(Cursor::new(data));
    // Must not panic on underflow
    let _ = codec_ape::open_ape(cursor, &ctx.codecs);
}

#[test]
fn test_p1_nblocks_cap() {
    let _ctx = RuntimeContext::new();
    let mut params = CodecParameters::audio(oxideav_core::CodecId::new("ape"));
    let mut extradata = vec![0u8; 6];
    extradata[0..2].copy_from_slice(&3990u16.to_le_bytes());
    extradata[2..4].copy_from_slice(&2000u16.to_le_bytes());
    params.extradata = extradata;
    params.sample_rate = Some(44100);
    params.channels = Some(2);

    let mut decoder = codec_ape::make_decoder(&params).expect("make decoder");

    // Packet with oversize nblocks (e.g. 300_000_000, which exceeds FFmpeg's INT_MAX/2/4 - 8)
    let mut pkt_data = vec![0u8; 32];
    let oversize_nblocks = 300_000_000u32;
    pkt_data[0..4].copy_from_slice(&oversize_nblocks.to_le_bytes());
    pkt_data[4..8].copy_from_slice(&0u32.to_le_bytes()); // skip = 0

    let packet = Packet::new(0, TimeBase::from_rate(44100), pkt_data);
    let _ = decoder.send_packet(&packet);

    // Should reject the oversize nblocks and emit no frames
    assert!(decoder.receive_frame().is_err());
}

#[test]
fn test_p1_predictor_decode_mono_3950_truncation() {
    let mut p = codec_ape::predictor::APEPredictor64::default();
    p.coeffsA[0] = [360, 317, -109, 98];

    // Set large 24-bit history values so that the dot product exceeds 32 bits:
    // (8_388_607 * 360) = 3_019_898_520 (> i32::MAX = 2_147_483_647)
    let large_history = 0x007F_FFFFi64; // 8_388_607
    p.historybuffer[codec_ape::predictor::YDELAYA] = large_history;
    p.historybuffer[codec_ape::predictor::YDELAYA - 1] = large_history;
    p.historybuffer[codec_ape::predictor::YDELAYA - 2] = 0;
    p.historybuffer[codec_ape::predictor::YDELAYA - 3] = 0;

    let mut filters = std::array::from_fn(|i| {
        let order = codec_ape::filter::APE_FILTER_ORDERS[1][i];
        [codec_ape::filter::APEFilter::new(order), codec_ape::filter::APEFilter::new(order)]
    });

    let mut decoded = [100i32];
    codec_ape::predictor::predictor_decode_mono_3950(
        3990,
        1,
        &mut filters,
        &mut p,
        &mut decoded,
        1,
    );

    // Under FFmpeg's int32_t truncation semantics:
    // pred_a = 8_388_607 * 360 + 0 = 3_019_898_520 as uint64 -> truncated to int32 is -1_275_068_776
    // pred_a >> 10 = -1_275_068_776 >> 10 = -1_245_185
    // current_a = 100 + (-1_245_185) = -1_245_085
    // filterA[0] = current_a + (0 * 31 >> 5) = -1_245_085
    // Under FFmpeg's int32_t truncation semantics:
    // current_a = p.lastA[0] = 0 overwrites history[YDELAYA].
    // history[YDELAYA - 1] becomes 0 - 8_388_607 = -8_388_607.
    // pred_a = -8_388_607 * 317 = -2_659_188_419 (as uint64: 0xFFFF_FFFF_6115_F4BD).
    // Truncated to int32: 0x6115_F4BD = 1_635_778_877.
    // pred_a >> 10 = 1_635_778_877 >> 10 = 1_597_440.
    // current_a = 100 + 1_597_440 = 1_597_540.
    // filterA[0] = 1_597_540.
    // (Without int32 truncation, 64-bit gives -2_659_188_419 >> 10 = -2_596_864, decoded = -2_596_764).
    let expected_ffmpeg_int32 = 1_597_540i32;
    assert_eq!(
        decoded[0], expected_ffmpeg_int32,
        "predictor_decode_mono_3950 must match FFmpeg's int32_t truncation semantics"
    );
}
