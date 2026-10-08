//! Durable, completed-input records for shared-machine quiet windows. An
//! interrupted input has no record: --resume repeats all of its timed runs.

use super::{Input, Kind, Measured, Options};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct InputStamp {
    rows: Vec<String>,
    source: String,
    path: String,
    kind: Kind,
    stream: Option<u32>,
    decoder: Option<String>,
    bytes: Option<u64>,
    modified_ns: Option<u128>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Header {
    workspace_commit: String,
    harness_md5: String,
    release: bool,
    runs: usize,
    max_secs: f64,
    check: bool,
    ffmpeg: bool,
    inputs: Vec<InputStamp>,
}

pub struct Journal {
    file: File,
    header: Header,
}

impl Journal {
    pub fn open(opts: &Options, inputs: &[Input]) -> (Vec<Measured>, Self) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let git = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(root)
            .output().expect("read benchmark revision");
        assert!(git.status.success(), "git rev-parse HEAD failed");
        let mut hash = md5::Context::new();
        hash.consume(include_bytes!("main.rs"));
        hash.consume(include_bytes!("pcm.rs"));
        hash.consume(include_bytes!("journal.rs"));
        hash.consume(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml")));
        hash.consume(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")));
        let mut stamps: Vec<_> = inputs.iter().map(|input| {
            let metadata = std::fs::metadata(&input.path).ok();
            InputStamp {
                rows: input.rows.clone(),
                source: input.source.clone(),
                path: input.path.to_string_lossy().into_owned(),
                kind: input.kind,
                stream: input.stream.as_ref().map(|s| s.index),
                decoder: input.stream.as_ref().and_then(|s| s.decoder.clone()),
                bytes: metadata.as_ref().map(|m| m.len()),
                modified_ns: metadata.and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()),
            }
        }).collect();
        stamps.sort_by(|a, b| a.rows.cmp(&b.rows).then_with(|| a.path.cmp(&b.path)));
        let header = Header {
            workspace_commit: String::from_utf8(git.stdout).expect("UTF-8 git revision").trim().to_owned(),
            harness_md5: format!("{:x}", hash.compute()),
            release: !cfg!(debug_assertions),
            runs: opts.runs,
            max_secs: opts.max_secs,
            check: opts.check,
            ffmpeg: opts.ffmpeg,
            inputs: stamps,
        };
        let path = opts.out.with_extension("jsonl");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create output directory");
        }
        if opts.resume {
            let data = std::fs::read(&path).expect("read --resume journal");
            // Only newline-terminated records were fully written. A signal may
            // have interrupted the final write; never reuse that partial entry.
            let end = data.iter().rposition(|b| *b == b'\n').map(|i| i + 1)
                .expect("journal has no complete header");
            let text = std::str::from_utf8(&data[..end]).expect("UTF-8 journal");
            let mut lines = text.lines();
            let saved: Header = serde_json::from_str(lines.next().unwrap()).expect("journal header");
            assert_eq!(saved, header, "cannot resume: revision, harness, options, or inputs changed");
            let results: Vec<Measured> = lines.map(|line| serde_json::from_str(line).expect("complete journal record")).collect();
            for (index, result) in results.iter().enumerate() {
                assert!(header.inputs.iter().any(|input| input.rows == result.rows && input.source == result.input && input.path == result.path),
                    "journal result is not a selected input");
                assert!(!results[..index].iter().any(|old| old.rows == result.rows && old.input == result.input && old.path == result.path),
                    "duplicate completed journal input");
            }
            let file = OpenOptions::new().write(true).append(true).open(&path).expect("open resume journal");
            file.set_len(end as u64).expect("discard partial final journal record");
            eprintln!("resuming {} completed inputs from {}", results.len(), path.display());
            (results, Self { file, header })
        } else {
            let mut file = File::create(&path).expect("create result journal");
            serde_json::to_writer(&mut file, &header).expect("write journal header");
            file.write_all(b"\n").expect("finish journal header");
            file.sync_data().expect("persist journal header");
            (Vec::new(), Self { file, header })
        }
    }

    pub fn append(&mut self, measured: &Measured) {
        serde_json::to_writer(&mut self.file, measured).expect("write completed input");
        self.file.write_all(b"\n").expect("finish completed input");
        self.file.sync_data().expect("persist completed input");
    }

    pub fn workspace_commit(&self) -> &str {
        &self.header.workspace_commit
    }

    pub fn harness_md5(&self) -> &str {
        &self.header.harness_md5
    }
}
