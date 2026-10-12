//! Over HTTP the player reads through `oxideav_http::HttpSource`, which
//! refuses a seek past the end of the resource (`InvalidInput`), where a
//! file allows it and then reads nothing. Cut ASF files declare more than
//! they hold: the demuxer must end where the bytes end, cleanly and with the
//! same packets, through either reader.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use oxideav_core::{Error, ReadSeek};

/// A file that refuses seeks past its end, as `HttpSource` does.
struct NoSeekPastEnd {
    file: File,
    len: u64,
}

impl Read for NoSeekPastEnd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for NoSeekPastEnd {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let pos = self.file.stream_position()? as i128;
        let target = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(d) => pos + i128::from(d),
            SeekFrom::End(d) => i128::from(self.len) + i128::from(d),
        };
        if !(0..=i128::from(self.len)).contains(&target) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek past end"));
        }
        self.file.seek(SeekFrom::Start(target as u64))
    }
}

/// Every packet (stream, pts, size) to the end, or the error that stopped it.
fn packets(input: Box<dyn ReadSeek>) -> Result<Vec<(u32, Option<i64>, usize)>, String> {
    let mut ctx = oxideav_core::RuntimeContext::new();
    demux_asf::register(&mut ctx);
    let mut demuxer = ctx.containers.open_demuxer("asf", input, &ctx.codecs).map_err(|e| format!("open: {e}"))?;
    let mut out = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => out.push((p.stream_index, p.pts, p.data.len())),
            Err(Error::Eof) => return Ok(out),
            Err(e) => return Err(format!("after {} packets: {e}", out.len())),
        }
    }
}

#[test]
fn cut_files_end_where_the_bytes_end_through_a_reader_refusing_seeks_past_it() {
    for sample in ["lossless-audio/luckynight-partial.wma", "wmapro/Beethovens_9th-1_small.wma"] {
        let path = refcheck::fate(sample);
        let from_file = packets(Box::new(File::open(&path).unwrap())).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let strict = packets(Box::new(NoSeekPastEnd { file: File::open(&path).unwrap(), len }));
        assert_eq!(strict.as_ref(), Ok(&from_file), "{sample}");
    }
}
