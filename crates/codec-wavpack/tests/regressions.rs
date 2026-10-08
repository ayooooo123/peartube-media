// Ported from FFmpeg libavcodec/wavpack.c, libavformat/wvdec.c (commit 2da55bf)
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

use std::fs::File;
use std::process::Command;

use oxideav_core::{CodecId, CodecParameters, Decoder, Demuxer, Frame, MediaType, Packet, TimeBase};
use refcheck::{decode, pinned_ffmpeg};

fn make_block(samples: u32, flags: u32, subblocks: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"wvpk");
    let blocksize = 24 + subblocks.len() as u32;
    data.extend_from_slice(&blocksize.to_le_bytes());
    data.extend_from_slice(&0x410u16.to_le_bytes()); // version 4.10
    data.extend_from_slice(&0u8.to_le_bytes()); // track_no
    data.extend_from_slice(&0u8.to_le_bytes()); // index_no
    data.extend_from_slice(&samples.to_le_bytes()); // total_samples
    data.extend_from_slice(&0u32.to_le_bytes()); // block_idx
    data.extend_from_slice(&samples.to_le_bytes()); // samples
    data.extend_from_slice(&flags.to_le_bytes()); // flags
    data.extend_from_slice(&0u32.to_le_bytes()); // crc
    data.extend_from_slice(subblocks);
    data
}

fn drain(dec: &mut Box<dyn Decoder>) {
    while dec.receive_frame().is_ok() {}
}

#[test]
fn regression_mono_negative_decorr_term() {
    // Before fix: `(pos + t as usize) & 7` in wv_unpack_mono panicked with
    // "attempt to add with overflow" when pos >= 1 and t in [-5, -1].
    // Value = (byte & 0x1F) - 5. For byte 4, value = -1.
    let mut sub = Vec::new();
    // DECTERMS: 1 term with value -1 (byte 4)
    sub.extend_from_slice(&[2, 1, 4, 0]); // sub-block 2, size 1 word (padded)
    // DECWEIGHTS: 1 weight (0)
    sub.extend_from_slice(&[3, 1, 0, 0]);
    // DECSAMPLES: 1 sample
    sub.extend_from_slice(&[4, 1, 0, 0]);
    // ENTROPY: 3 medians (0, 0, 0)
    sub.extend_from_slice(&[5, 3, 0, 0, 0, 0, 0, 0]);
    // DATA: 10 zero bits
    sub.extend_from_slice(&[10, 2, 0, 0, 0, 0]);

    // Mono flag = 4, INITIAL = 0x800, FINAL = 0x1000 -> 0x1804
    let block = make_block(5, 0x1804, &sub);
    let pkt = Packet::new(0, TimeBase::new(1, 44100), block);

    let params = CodecParameters::audio(CodecId::new("wavpack"));
    let mut dec = codec_wavpack::decoder::make_decoder(&params).expect("make decoder");
    let _ = dec.send_packet(&pkt);
    drain(&mut dec);
}

#[test]
fn regression_chaninfo_4096_channels_cap() {
    // Before fix: WP_ID_CHANINFO size-2 == 4 allowed chan to reach 4096 channels,
    // bypassing the 64-channel cap and attempting to allocate 2.4 GiB.
    let mut sub = Vec::new();
    // WP_ID_CHANINFO: id 0x0D, size 6 bytes (3 words)
    // sub_data[0] = 0xFF, sub_data[1] = 0, sub_data[2] = 0x0F (chan |= 0xF << 8; chan += 1 => 4096)
    sub.extend_from_slice(&[0x0D, 3, 0xFF, 0x00, 0x0F, 0x00, 0x00, 0x00]);
    // ENTROPY
    sub.extend_from_slice(&[5, 3, 0, 0, 0, 0, 0, 0]);
    // DATA
    sub.extend_from_slice(&[10, 1, 0, 0]);

    // Multiblock flag: INITIAL (0x800), not FINAL
    let block = make_block(10, 0x800, &sub);
    let pkt = Packet::new(0, TimeBase::new(1, 44100), block);

    let params = CodecParameters::audio(CodecId::new("wavpack"));
    let mut dec = codec_wavpack::decoder::make_decoder(&params).expect("make decoder");
    // Must error on channel count > 64 and NOT allocate 2.4 GiB or panic
    let res = dec.send_packet(&pkt);
    assert!(res.is_err(), "decoder must reject 4096 channels from CHANINFO");
}

#[test]
fn regression_dsd_false_stereo_fewer_channels() {
    // Before fix: DSD false stereo copy indexed dsd_scratch out of bounds when
    // CHANINFO declared fewer channels than block carried.
    let mut sub = Vec::new();
    // CHANINFO: 1 channel, mask 4 (mono mask)
    sub.extend_from_slice(&[0x0D, 1, 1, 4]);
    // DSD DATA: mode 0 (copy), rate_x 0
    sub.extend_from_slice(&[0x0E, 2, 0, 0, 0x69, 0x69]);

    // Flags: INITIAL (0x800) | DSD (0x8000_0000) | FALSE_STEREO (0x4000_0000) (not MONO)
    let block = make_block(2, 0xC000_0800, &sub);
    let pkt = Packet::new(0, TimeBase::new(1, 44100), block);

    let params = CodecParameters::audio(CodecId::new("wavpack"));
    let mut dec = codec_wavpack::decoder::make_decoder(&params).expect("make decoder");
    let res = dec.send_packet(&pkt);
    // Must reject or handle without panic
    if res.is_ok() {
        drain(&mut dec);
    }
}

#[test]
fn regression_escape_add_overflow() {
    // Before fix: t += bits | (1 << (t2 - 1)) overflowed signed i32 when t = 16 and t2 = 31.
    let mut sub = Vec::new();
    // DECTERMS: 0 terms
    sub.extend_from_slice(&[2, 0]);
    // DECWEIGHTS: 0
    sub.extend_from_slice(&[3, 0]);
    // DECSAMPLES: 0
    sub.extend_from_slice(&[4, 0]);
    // ENTROPY: medians [0, 0, 0]
    sub.extend_from_slice(&[5, 3, 0, 0, 0, 0, 0, 0]);

    // Bitstream:
    // 16 ones (t = 16), then 0 bit (terminates unary)
    // 31 ones (t2 = 31), then 0 bit (terminates unary)
    // 30 ones (bits)
    // Followed by tail bits
    let mut bits = Vec::new();
    // 16 ones = 0xFFFF (2 bytes)
    bits.extend_from_slice(&[0xFF, 0xFF]);
    // bit 16 is 0, bits 17..31 are 1 (15 ones), bits 32..47 are 1 (16 ones) -> total 31 ones
    // byte 2: bit 0 is 0, bits 1..7 are 1 (0xFE)
    // byte 3: bits 0..7 are 1 (0xFF)
    // byte 4: bits 0..7 are 1 (0xFF)
    // byte 5: bits 0..7 are 1 (0xFF)
    // byte 6: bit 0 is 0 (terminates t2), remaining bits 1
    bits.extend_from_slice(&[0xFE, 0xFF, 0xFF, 0xFF, 0xFE, 0xFF, 0xFF, 0xFF, 0xFF]);

    let mut data_sub = Vec::new();
    let word_len = (bits.len() + 1) / 2;
    data_sub.push(10); // WP_ID_DATA
    data_sub.push(word_len as u8);
    data_sub.extend_from_slice(&bits);
    if data_sub.len() % 2 != 0 {
        data_sub.push(0);
    }
    sub.extend_from_slice(&data_sub);

    let block = make_block(1, 0x1804, &sub); // mono single block
    let pkt = Packet::new(0, TimeBase::new(1, 44100), block);

    let params = CodecParameters::audio(CodecId::new("wavpack"));
    let mut dec = codec_wavpack::decoder::make_decoder(&params).expect("make decoder");
    let _ = dec.send_packet(&pkt);
    drain(&mut dec);
}

#[test]
fn regression_base_wrapping_add_overflow() {
    // Before fix: base = med0 + med1 + med2*(t-2) overflowed signed i32 with large medians.
    let mut sub = Vec::new();
    sub.extend_from_slice(&[2, 0]);
    sub.extend_from_slice(&[3, 0]);
    sub.extend_from_slice(&[4, 0]);
    // Large medians in WP_ID_ENTROPY: 0x1FFF -> wp_exp2 gives large positive number
    sub.extend_from_slice(&[5, 3, 0xFF, 0x1F, 0xFF, 0x1F, 0xFF, 0x1F]);
    // DATA with unary t > 2
    sub.extend_from_slice(&[10, 2, 0xFF, 0x0F, 0x00, 0x00]);

    let block = make_block(1, 0x1804, &sub);
    let pkt = Packet::new(0, TimeBase::new(1, 44100), block);

    let params = CodecParameters::audio(CodecId::new("wavpack"));
    let mut dec = codec_wavpack::decoder::make_decoder(&params).expect("make decoder");
    let _ = dec.send_packet(&pkt);
    drain(&mut dec);
}

#[test]
fn regression_hybrid_bisection_sub_overflow() {
    // Before fix: `add -= mid - base` overflowed when base was large negative.
    let mut sub = Vec::new();
    sub.extend_from_slice(&[2, 0]);
    sub.extend_from_slice(&[3, 0]);
    sub.extend_from_slice(&[4, 0]);
    // Median 0 is negative (wp_exp2(-0x1FFF)), median 1 positive
    sub.extend_from_slice(&[5, 3, 0x01, 0xE0, 0xFF, 0x1F, 0x00, 0x00]);
    // HYBRID sub-block: error_limit = 1
    sub.extend_from_slice(&[6, 2, 0x00, 0x01, 0x00, 0x00]);
    // DATA
    sub.extend_from_slice(&[10, 2, 0x03, 0x00, 0xFF, 0xFF]);

    // Flags: HYBRID_MODE (8), MONO (4) -> 0x180C
    let block = make_block(1, 0x180C, &sub);
    let pkt = Packet::new(0, TimeBase::new(1, 44100), block);

    let params = CodecParameters::audio(CodecId::new("wavpack"));
    let mut dec = codec_wavpack::decoder::make_decoder(&params).expect("make decoder");
    let _ = dec.send_packet(&pkt);
    drain(&mut dec);
}

#[test]
fn regression_custom_sample_rate() {
    // Test custom sample rate 176400 Hz (outside the table, uses WP_ID_SAMPLE_RATE).
    // Generate temporary fixture using ffmpeg.
    let path = std::env::temp_dir().join("test_custom_176400.wv");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=176400:duration=0.1",
            "-c:a",
            "wavpack",
        ])
        .arg(&path)
        .status()
        .expect("ffmpeg runs to generate fixture");
    assert!(status.success(), "ffmpeg fixture generation succeeds");

    // 1. Demuxer check: must correctly parse rate 176400 Hz, not 0
    let file = File::open(&path).expect("open file");
    let ctx = oxideav_core::RuntimeContext::new();
    let demuxer = codec_wavpack::demuxer::RawWvDemuxer::open(Box::new(file), &ctx.codecs)
        .expect("open demuxer");
    assert_eq!(
        demuxer.streams()[0].params.sample_rate,
        Some(176400),
        "demuxer parses custom sample rate 176400"
    );

    // 2. Decoder check: decode through refcheck and compare against pinned FFmpeg -cpuflags 0
    let decoded = decode(&path, &[codec_wavpack::register], MediaType::Audio, 0);
    let format = decoded.audio_format.expect("output audio format reported");
    assert_eq!(format.sample_rate, 176400, "reported sample rate");

    let mut our_pcm = Vec::new();
    for frame in &decoded.frames {
        if let Frame::Audio(audio) = frame {
            let bpp = format.sample_format.bytes_per_sample();
            for i in 0..audio.samples as usize {
                for c in 0..audio.data.len() {
                    let off = i * bpp;
                    our_pcm.extend_from_slice(&audio.data[c][off..off + bpp]);
                }
            }
        }
    }

    let theirs_out = Command::new(pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0"])
        .args(["-i", path.to_str().unwrap()])
        .args(["-map", "0:a:0", "-f", "s16le", "-c:a", "pcm_s16le", "-"])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(theirs_out.status.success());
    let their_pcm = theirs_out.stdout;

    assert_eq!(our_pcm.len(), their_pcm.len(), "byte length match");
    assert_eq!(our_pcm, their_pcm, "bit-exact match against pinned FFmpeg on 176400 Hz file");

    let _ = std::fs::remove_file(&path);
    eprintln!("custom sample rate 176400 Hz: bit-exact match against pinned FFmpeg");
}
