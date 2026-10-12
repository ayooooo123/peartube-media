// Ported from FFmpeg libavformat/mxfdec.c (commit 2da55bf): klv_decode_ber_length,
// mxf_read_sync, mxf_read_sync_klv, klv_read_packet; the avio reads they use.
// License: LGPL-2.1-or-later

//! The input as FFmpeg's avio reads it: past the end a read gives zeros
//! and sets end-of-file, which callers check where FFmpeg does.

use std::io::{BufReader, Read, Seek, SeekFrom};

use oxideav_core::{Error, ReadSeek, Result};

use crate::types::{Uid, KLV_KEY};

pub struct Io {
    inner: BufReader<Box<dyn ReadSeek>>,
    pos: u64,
    /// avio_feof: a read reached the end, or failed.
    eof: bool,
    /// The first read error, which ends reading as the end of the input
    /// does and is reported where a read must succeed.
    error: Option<std::io::Error>,
}

impl Io {
    pub fn new(inner: Box<dyn ReadSeek>) -> Self {
        Self { inner: BufReader::with_capacity(64 * 1024, inner), pos: 0, eof: false, error: None }
    }

    pub fn tell(&self) -> u64 {
        self.pos
    }

    pub fn feof(&self) -> bool {
        self.eof
    }

    /// avio_seek(SEEK_SET): clears end-of-file.
    pub fn seek(&mut self, to: u64) -> Result<u64> {
        if to == self.pos && self.error.is_none() {
            self.eof = false;
            return Ok(to);
        }
        let delta = i64::try_from(to.wrapping_sub(self.pos)).ok().filter(|_| to >= self.pos);
        match delta {
            Some(d) if d <= self.inner.buffer().len() as i64 => self.inner.seek_relative(d)?,
            _ => {
                self.inner.seek(SeekFrom::Start(to))?;
            }
        }
        self.pos = to;
        self.eof = false;
        self.error = None;
        Ok(to)
    }

    /// avio_skip: a forward or backward seek relative to here.
    pub fn skip(&mut self, by: i64) -> Result<u64> {
        let to = self.pos.checked_add_signed(by).ok_or_else(|| Error::invalid("mxf: seek before the start"))?;
        self.seek(to)
    }

    /// avio_size.
    pub fn size(&mut self) -> Result<u64> {
        let end = self.inner.seek(SeekFrom::End(0))?;
        self.inner.seek(SeekFrom::Start(self.pos))?;
        Ok(end)
    }

    /// avio_read: up to `buf.len()` bytes, fewer at the end.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut got = 0;
        while got < buf.len() && !self.eof {
            match self.inner.read(&mut buf[got..]) {
                Ok(0) => self.eof = true,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.eof = true;
                    self.error = Some(e);
                }
            }
        }
        self.pos += got as u64;
        got
    }

    /// The read error that ended reading, if one did.
    pub fn take_error(&mut self) -> Option<Error> {
        self.error.take().map(Error::Io)
    }

    /// ffio_read_size: all of `buf`, else an error.
    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        if self.read(buf) == buf.len() {
            return Ok(());
        }
        Err(self.take_error().unwrap_or(Error::Eof))
    }

    pub fn r8(&mut self) -> u8 {
        let mut b = [0u8; 1];
        self.read(&mut b);
        b[0]
    }

    pub fn rb16(&mut self) -> u16 {
        let mut b = [0u8; 2];
        self.read(&mut b);
        u16::from_be_bytes(b)
    }

    pub fn rb32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.read(&mut b);
        u32::from_be_bytes(b)
    }

    pub fn rb64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.read(&mut b);
        u64::from_be_bytes(b)
    }

    pub fn uid(&mut self) -> Uid {
        let mut b = [0u8; 16];
        self.read(&mut b);
        b
    }

    /// av_get_packet: up to `len` bytes; fewer at the end of the input.
    pub fn get_packet(&mut self, len: u64) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut left = len;
        let mut chunk = [0u8; 64 * 1024];
        while left > 0 {
            let want = left.min(chunk.len() as u64) as usize;
            let got = self.read(&mut chunk[..want]);
            data.extend_from_slice(&chunk[..got]);
            left -= got as u64;
            if got < want {
                break;
            }
        }
        if data.is_empty() && len > 0 {
            return Err(self.take_error().unwrap_or(Error::Eof));
        }
        Ok(data)
    }
}

/// A KLV packet: key, value length, where its key starts and where the
/// next one does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Klv {
    pub key: Uid,
    pub offset: u64,
    pub length: u64,
    pub next_klv: u64,
}

/// klv_decode_ber_length: the length and how many bytes coded it.
pub fn decode_ber_length(io: &mut Io) -> Result<(u64, u64)> {
    let mut size = u64::from(io.r8());
    let mut llen = 1;
    if size & 0x80 != 0 {
        let bytes_num = size & 0x7f;
        // SMPTE 379M 5.3.4 guarantees bytes_num does not exceed 8.
        if bytes_num > 8 {
            return Err(Error::invalid("mxf: BER length of more than 8 bytes"));
        }
        llen = bytes_num + 1;
        size = 0;
        for _ in 0..bytes_num {
            size = (size << 8) | u64::from(io.r8());
        }
    }
    if size > i64::MAX as u64 {
        return Err(Error::invalid("mxf: BER length past int64"));
    }
    Ok((size, llen))
}

/// mxf_read_sync: past the next occurrence of `key`; whether one was found.
pub fn read_sync(io: &mut Io, key: &[u8]) -> bool {
    let size = key.len() as i64;
    let mut i: i64 = 0;
    while i < size && !io.feof() {
        let b = io.r8();
        if b == key[0] {
            i = 0;
        } else if b != key[i as usize] {
            i = -1;
        }
        i += 1;
    }
    i == size
}

/// mxf_read_sync_klv: past the next 06 0e 2b 34.
fn read_sync_klv(io: &mut Io) -> bool {
    let want = u32::from_be_bytes(KLV_KEY);
    let mut key = io.rb32();
    if key == want && !io.feof() {
        return true;
    }
    while !io.feof() {
        key = (key << 8) | u32::from(io.r8());
        if key == want && !io.feof() {
            return true;
        }
    }
    false
}

/// klv_read_packet: the next KLV from here, its value next to read.
pub fn read_packet(io: &mut Io, run_in: u64) -> Result<Klv> {
    if !read_sync_klv(io) {
        return Err(io.take_error().unwrap_or(Error::Eof));
    }
    let offset = io.tell() - 4;
    if offset < run_in {
        return Err(Error::invalid("mxf: KLV before the run-in"));
    }
    let mut key = [0u8; 16];
    key[..4].copy_from_slice(&KLV_KEY);
    io.read_exact(&mut key[4..])?;
    let (length, llen) = decode_ber_length(io)?;
    if offset > i64::MAX as u64 - 16 - llen {
        return Err(Error::invalid("mxf: KLV offset overflows"));
    }
    let pos = offset + 16 + llen;
    if pos > i64::MAX as u64 - length {
        return Err(Error::invalid("mxf: KLV length overflows"));
    }
    Ok(Klv { key, offset, length, next_klv: pos + length })
}
