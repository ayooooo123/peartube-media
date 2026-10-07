//! Complete MPEG decode/oracle and serial timing through the production registry.
//! cargo run --release -p codecs --example mpeg12 -- [--runs 3] INPUT...
use oxideav_core::{Decoder, Error, Frame, MediaType, PixelFormat};
use std::{fs::File, path::{Path, PathBuf}, time::Instant, process::Command};

#[derive(Debug)]
pub struct Report {
    pub frames: usize,
    pub before_eof: usize,
    pub first_output_packet: Option<usize>,
    pub geometry_packet: Option<usize>,
    pub geometry_bytes: Option<usize>,
    pub initial_dimensions: Option<(u32,u32)>,
    pub opened_dimensions: (Option<u32>,Option<u32>),
    pub packets: usize,
    pub input_bytes: usize,
    pub hashes: Vec<String>,
    pub pts: Vec<Option<i64>>,
    pub stamped_packets: usize,
    pub container: String,
    pub format: Option<PixelFormat>,
    pub elapsed: f64,
}
pub fn decode(path: &Path, hashes: bool, raw: Option<&Path>) -> Result<Report,String> {
    use std::io::Write;
    let started = Instant::now();
    let ctx = codecs::context();
    let format = refcheck::probe_container(&ctx,path)?;
    let mut demux = ctx.containers.open_demuxer(&format, Box::new(File::open(path).map_err(|e|e.to_string())?), &ctx.codecs).map_err(|e|format!("open {format}: {e}"))?;
    let stream = demux.streams().iter().find(|s|s.params.media_type == MediaType::Video).ok_or("no video stream")?.clone();
    if !["mpeg1video","mpeg2video"].contains(&stream.params.codec_id.as_str()) { return Err(format!("not MPEG12: {:?}", stream.params.codec_id)); }
    let mut decoder = ctx.codecs.first_decoder(&stream.params).map_err(|e|e.to_string())?;
    let mut report = Report {frames:0,before_eof:0,first_output_packet:None,geometry_packet:None,geometry_bytes:None,initial_dimensions:None,opened_dimensions:(stream.params.width,stream.params.height),packets:0,input_bytes:0,hashes:Vec::new(),pts:Vec::new(),stamped_packets:0,container:format.clone(),format:None,elapsed:0.0};
    let mut raw = raw.map(File::create).transpose().map_err(|e|e.to_string())?;
    let drain = |decoder: &mut dyn Decoder, report: &mut Report, raw: &mut Option<File>| -> Result<(),String> {
        loop {
            match decoder.receive_frame() {
                Ok(Frame::Video(frame)) => {
                    let (w,h) = decoder.output_video_dimensions().ok_or("missing output dimensions")?;
                    let fmt = decoder.output_pixel_format().ok_or("missing output pixel format")?;
                    report.format = Some(fmt);
                    report.first_output_packet.get_or_insert(report.packets);
                    report.frames += 1;
                    if hashes || raw.is_some() {
                        let (sx,sy) = match fmt {PixelFormat::Yuv420P=>(1,1),PixelFormat::Yuv422P=>(1,0),PixelFormat::Yuv444P=>(0,0),_=>return Err(format!("unexpected format {fmt:?}"))};
                        let chroma = (((w as usize)+(1<<sx)-1)>>sx, ((h as usize)+(1<<sy)-1)>>sy);
                        let bytes = refcheck::pack(&frame,&[(w as usize,h as usize),chroma,chroma]);
                        if hashes { report.hashes.push(refcheck::md5_hex(&bytes)); }
                        if let Some(raw) = raw { raw.write_all(&bytes).map_err(|e|e.to_string())?; }
                    }
                    report.pts.push(frame.pts);
                    std::hint::black_box(frame);
                }
                Ok(_) => return Err("non-video frame".into()),
                Err(Error::NeedMore | Error::Eof) => return Ok(()),
                Err(e) => return Err(format!("receive after packet {} frame {}: {e}",report.packets,report.frames)),
            }
        }
    };
    loop {
        match demux.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                report.packets += 1;
                report.input_bytes += packet.data.len();
                report.stamped_packets += usize::from(packet.pts.is_some());
                decoder.send_packet(&packet).map_err(|e|format!("send packet {}: {e}",report.packets))?;
                if let Some(dims) = decoder.output_video_dimensions() {
                    if report.geometry_packet.is_none() {
                        report.geometry_packet = Some(report.packets);
                        report.geometry_bytes = Some(report.input_bytes);
                        report.initial_dimensions = Some(dims);
                    }
                }
                drain(decoder.as_mut(),&mut report,&mut raw)?;
            }
            Ok(_) => (), Err(Error::Eof) => break,
            Err(e) => return Err(format!("demux after packet {}: {e}",report.packets)),
        }
    }
    report.before_eof = report.frames;
    decoder.flush().map_err(|e|e.to_string())?;
    drain(decoder.as_mut(),&mut report,&mut raw)?;
    report.elapsed = started.elapsed().as_secs_f64();
    Ok(report)
}

pub fn compare(path: &Path, output: &Path) -> Result<Report,String> {
    let ours = decode(path,true,None)?;
    let fmt = ours.format.ok_or("no decoded frames")?;
    let reference = refcheck::ffmpeg_video_md5s_with(path,0,refcheck::ffmpeg_pix_fmt(fmt),&["-idct","simple"]);
    let matching = ours.hashes.iter().zip(&reference).filter(|(a,b)|a==b).count();
    let reference_pts = ffmpeg_best_effort(path)?;
    let mut table = String::from("frame\tdecoded_md5\tffmpeg_simple_md5\tpts\tffmpeg_best_effort\n");
    for i in 0..ours.hashes.len().max(reference.len()) {
        table.push_str(&format!("{i}\t{}\t{}\t{:?}\t{:?}\n",ours.hashes.get(i).map_or("MISSING",String::as_str),reference.get(i).map_or("MISSING",String::as_str),ours.pts.get(i),reference_pts.get(i)));
    }
    std::fs::write(output,table).map_err(|e|e.to_string())?;
    let pts_exact = ours.pts.iter().zip(&reference_pts).filter(|(a,b)|a==b).count();
    let ffmpeg_untimed = reference_pts.iter().filter(|t|t.is_none()).count();
    println!("{} frames={}/{} exact={} pre_eof={} first_packet={:?} geometry_packet={:?} geometry_bytes={:?} initial={:?} at_open={:?} packets={} stamped_packets={} bytes={} pts_exact={pts_exact}/{} ffmpeg_untimed={ffmpeg_untimed} oracle={}",path.display(),ours.frames,reference.len(),matching,ours.before_eof,ours.first_output_packet,ours.geometry_packet,ours.geometry_bytes,ours.initial_dimensions,ours.opened_dimensions,ours.packets,ours.stamped_packets,ours.input_bytes,reference_pts.len(),output.display());
    if ours.hashes != reference { return Err(format!("complete MD5 mismatch: {matching}/{} matched, {} decoded", reference.len(),ours.frames)); }
    if ours.frames > 2 && ours.before_eof == 0 { return Err("no incremental output".into()); }
    if ours.container == "mpegvideo" {
        // The raw demuxer numbers packets in coded order (1/fps); display
        // times must still rise by exactly one period after the first frame.
        let times: Vec<i64> = ours.pts.iter().map(|t| t.ok_or("untimed frame")).collect::<Result<_,_>>()?;
        if !times.windows(2).all(|w| w[0] < w[1]) || !times[1..].windows(2).all(|w| w[1] - w[0] == 1) {
            return Err(format!("raw display times not at frame cadence: {:?}", &times[..times.len().min(12)]));
        }
    } else {
        // Exact wherever FFmpeg has a time. FFmpeg leaves a flushed picture
        // without its own PTS untimed; ours must still continue the timeline.
        let wrong = (0..ours.pts.len().max(reference_pts.len())).find(|&i| match reference_pts.get(i) {
            Some(Some(t)) => ours.pts.get(i) != Some(&Some(*t)),
            Some(None) => !ours.pts.get(i).copied().flatten().is_some_and(|t| i == 0 || ours.pts[i-1].is_some_and(|p| t > p)),
            None => true,
        });
        if let Some(i) = wrong {
            return Err(format!("PTS mismatch at frame {i}: ours {:?}, FFmpeg {:?} ({pts_exact}/{} exact)", ours.pts.get(i), reference_pts.get(i), reference_pts.len()));
        }
    }
    Ok(ours)
}
fn ffmpeg_best_effort(path: &Path) -> Result<Vec<Option<i64>>,String> {
    let out = Command::new("ffprobe").args(["-v","error","-select_streams","v:0","-show_entries","frame=best_effort_timestamp","-of","csv=p=0"]).arg(path).output().map_err(|e|e.to_string())?;
    if !out.status.success() { return Err(String::from_utf8_lossy(&out.stderr).into_owned()); }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().trim_end_matches(',').parse().ok()).collect())
}
fn frame_rate(path: &Path) -> Result<f64,String> {
    let out = Command::new("ffprobe").args(["-v","error","-select_streams","v:0","-show_entries","stream=r_frame_rate","-of","default=noprint_wrappers=1:nokey=1"]).arg(path).output().map_err(|e|e.to_string())?;
    if !out.status.success() { return Err(String::from_utf8_lossy(&out.stderr).into_owned()); }
    let text = String::from_utf8(out.stdout).map_err(|e|e.to_string())?;
    let (n,d) = text.lines().next().ok_or("missing frame rate")?.split_once('/').ok_or("invalid frame rate")?;
    Ok(n.parse::<f64>().map_err(|e|e.to_string())? / d.parse::<f64>().map_err(|e|e.to_string())?)
}
fn main() {
    let mut args = std::env::args().skip(1);
    let mut runs = 0;
    let mut paths = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--runs" { runs = args.next().expect("run count").parse::<usize>().unwrap(); }
        else { paths.push(PathBuf::from(arg)); }
    }
    assert!(!paths.is_empty(),"pass complete input paths");
    let dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).expect("CARGO_TARGET_DIR required").join("evidence");
    std::fs::create_dir_all(&dir).unwrap();
    let mut failures = Vec::new();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy();
        let result = (|| {
            let checked = compare(&path,&dir.join(format!("{name}.framemd5.tsv")))?;
            let fps = frame_rate(&path)?;
            for run in 1..=runs {
                let report = decode(&path,false,None)?;
                if report.frames != checked.frames { return Err("timed frame count changed".into()); }
                let rt = report.frames as f64/fps/report.elapsed;
                println!("TIMING path={:?} run={run} frames={} seconds={:.6} xRT={rt:.6}",path,report.frames,report.elapsed);
            }
            Ok::<_,String>(())
        })();
        if let Err(error) = result { eprintln!("FAIL {}: {error}",path.display()); failures.push(path); }
    }
    if !failures.is_empty() { std::process::exit(1); }
}
