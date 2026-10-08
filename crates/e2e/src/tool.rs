//! Bounded runs of the pinned FFmpeg command-line tools
//! (`refcheck::pinned_ffmpeg`, `refcheck::pinned_ffprobe`), and of the
//! system `ffmpeg` for the AV1 references the pinned build cannot give:
//! stdin closed, both pipes drained on their own threads (a full stderr
//! pipe must not stall a child writing tens of MiB of PCM to stdout), and
//! the child killed when it outlives its deadline (FFmpeg's subtitle
//! parsers can spin forever on malformed samples).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `ffmpeg -v error -nostdin <args>`; stdout on success.
pub fn ffmpeg(args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut all = vec!["-v".to_string(), "error".into(), "-nostdin".into()];
    all.extend_from_slice(args);
    run(&refcheck::pinned_ffmpeg().to_string_lossy(), &all, timeout)
}

/// `ffprobe -v error <args>`; stdout on success. `-nostdin` is an ffmpeg
/// option that ffprobe rejects, so stdin is only closed.
pub fn ffprobe(args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut all = vec!["-v".to_string(), "error".into()];
    all.extend_from_slice(args);
    run(&refcheck::pinned_ffprobe().to_string_lossy(), &all, timeout)
}

/// [`ffmpeg`] on FFmpeg's C code paths (`-cpuflags 0`).
pub fn ffmpeg_c(args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut all = vec!["-cpuflags".to_string(), "0".into()];
    all.extend_from_slice(args);
    ffmpeg(&all, timeout)
}

/// [`ffprobe`] on FFmpeg's C code paths (`-cpuflags 0`).
pub fn ffprobe_c(args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut all = vec!["-cpuflags".to_string(), "0".into()];
    all.extend_from_slice(args);
    ffprobe(&all, timeout)
}

/// [`ffmpeg`] as the system build (`refcheck::system_ffmpeg`), whose
/// libdav1d decodes the AV1 the pinned build cannot.
pub fn system_ffmpeg(args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut all = vec!["-v".to_string(), "error".into(), "-nostdin".into()];
    all.extend_from_slice(args);
    run(&refcheck::system_ffmpeg().to_string_lossy(), &all, timeout)
}

fn run(program: &str, args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("{program}: {e}")),
        }
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        let err = String::from_utf8_lossy(&err);
        let err = err.trim();
        // The last lines carry the reason; earlier ones repeat per-packet noise.
        let tail = &err[err.len().saturating_sub(600)..];
        Err(format!("{program} failed ({status}): {tail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ffprobe_runs_without_ffmpeg_only_options() {
        let path = refcheck::fate("sub/SubRip_capability_tester.srt");
        let out = ffprobe(
            &args(&["-show_entries", "stream=codec_name", "-of", "csv=p=0", path.to_str().unwrap()]),
            Duration::from_secs(30),
        )
        .expect("ffprobe");
        assert_eq!(String::from_utf8(out).unwrap().trim(), "subrip");
    }

    #[test]
    fn ffprobe_rejects_nostdin() {
        // The option the old subtitle helper passed: ffprobe has no such
        // option and fails before probing anything.
        let path = refcheck::fate("sub/SubRip_capability_tester.srt");
        let err = ffprobe(&args(&["-nostdin", path.to_str().unwrap()]), Duration::from_secs(30)).unwrap_err();
        assert!(err.contains("nostdin"), "{err}");
    }

    #[test]
    fn a_child_past_its_deadline_is_killed() {
        let started = Instant::now();
        let err = run("sleep", &args(&["30"]), Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
