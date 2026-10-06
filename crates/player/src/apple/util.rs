//! Shared helpers: thread-safety wrappers, NAL parameter-set parsing, and
//! CoreMedia block-buffer construction.

use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};

use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, CMBlockBuffer,
};

use crate::backend::SinkError;

/// Wraps `T` so it can be moved between the engine's threads. Only for
/// types whose Apple docs promise thread safety; see module docs of the
/// types that use it.
#[repr(transparent)]
pub struct SendSync<T>(pub T);
unsafe impl<T> Send for SendSync<T> {}
unsafe impl<T> Sync for SendSync<T> {}
impl<T: Clone> Clone for SendSync<T> {
    fn clone(&self) -> Self {
        SendSync(self.0.clone())
    }
}
impl<T> Deref for SendSync<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}
impl<T> DerefMut for SendSync<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// Extracts the body of a FourCC-length atom (`fourcc`, `size`, payload)
/// from `data` if it is present at the top level. MP4/QTFF extradata blobs
/// are a run of such atoms.
pub fn find_atom<'a>(data: &'a [u8], fourcc: &[u8; 4]) -> Option<&'a [u8]> {
    let mut p = 0usize;
    while p + 8 <= data.len() {
        let size = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
        if size < 8 || p + size > data.len() {
            return None;
        }
        if &data[p + 4..p + 8] == fourcc {
            return Some(&data[p + 8..p + size]);
        }
        p += size;
    }
    None
}

/// Parses an `AVCDecoderConfigurationRecord` (`avcC` box body, ISO/IEC
/// 14496-15). Returns the SPS and PPS NAL units (emulation-prevention
/// bytes still present — the hardware path wants them) and the NAL length
/// size in bytes (1, 2, or 4).
pub fn parse_avcc(rec: &[u8]) -> Option<(Vec<&[u8]>, usize)> {
    if rec.len() < 7 || rec[0] != 1 {
        return None;
    }
    let nal_len = ((rec[4] & 0x03) + 1) as usize;
    let num_sps = (rec[5] & 0x1f) as usize;
    let mut pos = 6;
    let mut nals = Vec::new();
    for _ in 0..num_sps {
        if pos + 2 > rec.len() {
            return None;
        }
        let len = u16::from_be_bytes([rec[pos], rec[pos + 1]]) as usize;
        pos += 2;
        let end = pos.checked_add(len)?;
        if end > rec.len() {
            return None;
        }
        nals.push(&rec[pos..end]);
        pos = end;
    }
    if pos >= rec.len() {
        return None;
    }
    let num_pps = rec[pos] as usize;
    pos += 1;
    for _ in 0..num_pps {
        if pos + 2 > rec.len() {
            return None;
        }
        let len = u16::from_be_bytes([rec[pos], rec[pos + 1]]) as usize;
        pos += 2;
        let end = pos.checked_add(len)?;
        if end > rec.len() {
            return None;
        }
        nals.push(&rec[pos..end]);
        pos = end;
    }
    Some((nals, nal_len))
}

/// Parses an `HEVCDecoderConfigurationRecord` (`hvcC` box body, ISO/IEC
/// 14496-15). Returns every parameter-set NAL unit (VPS/SPS/PPS/SEI per
/// the arrays) and the NAL length size in bytes.
pub fn parse_hvcc(rec: &[u8]) -> Option<(Vec<&[u8]>, usize)> {
    if rec.len() < 23 || rec[0] != 1 {
        return None;
    }
    let nal_len = ((rec[21] & 0x03) + 1) as usize;
    let num_arrays = rec[22] as usize;
    let mut pos = 23;
    let mut nals = Vec::new();
    for _ in 0..num_arrays {
        if pos + 3 > rec.len() {
            return None;
        }
        let count = u16::from_be_bytes([rec[pos + 1], rec[pos + 2]]) as usize;
        pos += 3;
        for _ in 0..count {
            if pos + 2 > rec.len() {
                return None;
            }
            let len = u16::from_be_bytes([rec[pos], rec[pos + 1]]) as usize;
            pos += 2;
            let end = pos.checked_add(len)?;
            if end > rec.len() {
                return None;
            }
            nals.push(&rec[pos..end]);
            pos = end;
        }
    }
    Some((nals, nal_len))
}

/// Rewrites Annex-B input (`00 00 01` / `00 00 00 01` start codes) into the
/// 4-byte length-prefixed form VideoToolbox wants. Already-length-prefixed
/// input passes through unchanged (a copy, so the caller gets an owned
/// buffer either way). `length_size` is the avcC/hvcC NAL length size.
///
/// A single Annex-B NAL never carries a start code in its middle, so
/// scanning for every `00 00 01` is exact — except a genuine 3-byte
/// emulation of one inside NAL payload data, which cannot occur: H.264/HEVC
/// emulation prevention guarantees the byte sequences 00 00 0x (x ≤ 3) only
/// appear as start codes.
pub fn annex_b_to_length_prefixed(data: &[u8], length_size: usize) -> Vec<u8> {
    // A 4-byte-length-prefix first word only looks like Annex-B if the
    // length itself is 0/1 (nonsensical) — so this heuristic is safe.
    let is_annex_b = data.len() >= 3 && (data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]));
    if !is_annex_b {
        return data.to_vec();
    }

    // Collect (start_of_nal, end_of_nal) after each start code.
    let mut nal_ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    let mut cur_start: Option<usize> = None;
    while i < data.len() {
        if i + 3 <= data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = cur_start {
                nal_ranges.push((s, i));
            }
            i += 3;
            cur_start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = cur_start {
        nal_ranges.push((s, data.len()));
    }
    if nal_ranges.is_empty() {
        return data.to_vec();
    }

    let mut out = Vec::with_capacity(data.len());
    for (s, e) in nal_ranges {
        let nal = &data[s..e];
        if nal.is_empty() {
            continue;
        }
        let len = nal.len() as u32;
        match length_size {
            1 => out.push(len as u8),
            2 => out.extend_from_slice(&(len as u16).to_be_bytes()),
            _ => out.extend_from_slice(&len.to_be_bytes()),
        }
        out.extend_from_slice(nal);
    }
    out
}

/// Creates a `CMBlockBuffer` owning a copy of `data`.
///
/// # Safety
/// Only copies from `data`; the returned buffer must be released (drop the
/// `CFRetained`).
pub unsafe fn create_block_buffer_from_bytes(
    data: &[u8],
) -> Result<CFRetained<CMBlockBuffer>, SinkError> {
    if data.is_empty() {
        return Err(SinkError::Fatal("empty block buffer".into()));
    }
    let mut raw: *mut CMBlockBuffer = ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            data.len(),
            None,
            ptr::null(),
            0,
            data.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::new(&mut raw).expect("out pointer"),
        )
    };
    if status != 0 || raw.is_null() {
        return Err(SinkError::Fatal(format!(
            "CMBlockBufferCreateWithMemoryBlock: {status}"
        )));
    }
    // SAFETY: create_with_memory_block follows the Create rule (+1 count).
    let block = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(
            // SAFETY: non-empty slice, so the pointer is non-null.
            NonNull::new_unchecked(data.as_ptr() as *mut std::ffi::c_void),
            &block,
            0,
            data.len(),
        )
    };
    if status != 0 {
        return Err(SinkError::Fatal(format!(
            "CMBlockBufferReplaceDataBytes: {status}"
        )));
    }
    Ok(block)
}
