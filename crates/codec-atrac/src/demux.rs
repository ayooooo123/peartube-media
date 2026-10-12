// Helpers the AEA and OMA demuxers share: FFmpeg's `av_get_packet`
// reading, `av_rescale` rounding and `ff_pcm_read_seek` (libavformat/
// utils.c, pcm.c, libavutil/mathematics.c, FFmpeg commit 2da55bf).
// Copyright (c) the FFmpeg developers; LGPL-2.1-or-later (see LICENSE).

use std::io::SeekFrom;

use oxideav_core::{ReadSeek, Result};

/// `av_get_packet`: up to `size` bytes; fewer at the end of the input.
pub(crate) fn read_up_to(input: &mut dyn ReadSeek, size: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    let mut got = 0;
    while got < size {
        match input.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// `av_rescale(a, b, c)` for `a >= 0`: `a * b / c` rounded to nearest,
/// halves up.
pub(crate) fn rescale(a: i64, b: i64, c: i64) -> i64 {
    ((i128::from(a) * i128::from(b) + i128::from(c) / 2) / i128::from(c)) as i64
}

/// `ff_pcm_read_seek` with `AVSEEK_FLAG_BACKWARD`: the block at or before
/// `timestamp` (in 1/`rate` units), its first byte after `data_offset`
/// sought to. Returns the timestamp FFmpeg sets there (`cur_dts`).
pub(crate) fn pcm_seek(
    input: &mut dyn ReadSeek,
    data_offset: u64,
    timestamp: i64,
    rate: i64,
    block_align: i64,
    byte_rate: i64,
) -> Result<i64> {
    let ts = i128::from(timestamp.max(0));
    // av_rescale_rnd(ts * byte_rate, 1, rate * block_align, AV_ROUND_DOWN)
    let blocks = ts * i128::from(byte_rate) / (i128::from(rate) * i128::from(block_align));
    let pos = (blocks * i128::from(block_align)) as i64;
    input.seek(SeekFrom::Start(data_offset + pos as u64))?;
    Ok(rescale(pos, rate, byte_rate))
}

/// The input's length, the read position kept.
pub(crate) fn input_len(input: &mut dyn ReadSeek) -> Result<u64> {
    let at = input.stream_position()?;
    let len = input.seek(SeekFrom::End(0))?;
    input.seek(SeekFrom::Start(at))?;
    Ok(len)
}
