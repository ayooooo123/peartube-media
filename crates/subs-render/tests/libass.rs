//! Pixel references come from the system FFmpeg's libass, not pinned FFmpeg
//! (which has no libass). Both sides use the same hash-checked external fonts.
mod common;
use std::path::Path;
use std::process::Command;
use subs_render::{Image, Renderer, Track};

fn on_black(image: &Image, width: u32, height: u32) -> Vec<u8> {
    let mut rgb = vec![0; width as usize * height as usize * 3];
    for y in 0..image.height { for x in 0..image.width {
        let (dx, dy) = (image.x + x as i32, image.y + y as i32);
        if dx < 0 || dy < 0 || dx >= width as i32 || dy >= height as i32 { continue; }
        let src = &image.rgba[(y as usize * image.width as usize + x as usize) * 4..][..4];
        let dst = &mut rgb[(dy as usize * width as usize + dx as usize) * 3..][..3];
        for c in 0..3 { dst[c] = ((u16::from(src[c]) * u16::from(src[3]) + 127) / 255) as u8; }
    } }
    rgb
}
fn bbox(rgb: &[u8], width: usize) -> Option<[usize; 4]> {
    let mut b = [usize::MAX, usize::MAX, 0, 0];
    for (i, p) in rgb.chunks_exact(3).enumerate() {
        if p.iter().copied().max().unwrap() <= 8 { continue; }
        b[0] = b[0].min(i % width); b[1] = b[1].min(i / width);
        b[2] = b[2].max(i % width + 1); b[3] = b[3].max(i / width + 1);
    }
    (b[0] != usize::MAX).then_some(b)
}
fn compare(label: &str, actual: &[u8], expected: &[u8], width: usize) -> Option<String> {
    assert_eq!(actual.len(), expected.len());
    let (a, b) = (bbox(actual,width), bbox(expected,width));
    let (Some(a),Some(b)) = (a,b) else {
        return (a != b).then(|| format!("{label}: presence {a:?} vs {b:?}"));
    };
    let bounds_error = a.iter().zip(b).map(|(a,b)| a.abs_diff(b)).max().unwrap();
    // The union crop, not a mostly empty video frame: a missing or shifted
    // subtitle must not get an artificially high PSNR from the black area.
    let union = [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])];
    let mut squared = 0.0;
    let mut sums = [[0u64;3];2];
    for y in union[1]..union[3] { for x in union[0]..union[2] { for c in 0..3 {
        let i = (y * width + x) * 3 + c;
        squared += (f64::from(actual[i]) - f64::from(expected[i])).powi(2);
        sums[0][c] += u64::from(actual[i]); sums[1][c] += u64::from(expected[i]);
    } } }
    let pixels = ((union[2] - union[0]) * (union[3] - union[1])) as f64;
    let psnr = if squared == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 * pixels * 3.0 / squared).log10() };
    let colour_error = (0..3).map(|c| sums[0][c].abs_diff(sums[1][c]) as f64 / pixels).fold(0.0, f64::max);
    eprintln!("{label}: bounds {a:?}/{b:?}, colour {colour_error:.2}, crop PSNR {psnr:.2} dB");
    // Different curve coverage/FreeType rounding may cost a pixel. These
    // limits still reject wrong placement, missing borders and wrong colours.
    (bounds_error > 2 || colour_error > 8.0 || psnr < 20.0).then(|| format!("{label}: bounds error {bounds_error}, colour {colour_error:.2}, crop PSNR {psnr:.2}"))
}

fn oracle(path: &Path, time: i64, width: u32, height: u32) -> Vec<u8> {
    // Compare libass's RGBA colours, not FFmpeg's legacy ASS-to-TV-range
    // conversion (which maps even RGB white to 235 and black to 16).
    let filter = format!("format=rgb24,setparams=range=full,settb=1/1000,setpts={time},subtitles=filename='{}':fontsdir='{}':force_style='YCbCr Matrix=None',format=rgb24", path.display(), common::fonts().display());
    let out = Command::new(refcheck::system_ffmpeg()).args(["-v","error","-nostdin","-threads","1","-filter_threads","1","-f","lavfi","-i"])
        .arg(format!("color=c=black:s={width}x{height}:r=1"))
        .args(["-vf",&filter,"-frames:v","1","-f","rawvideo","-pix_fmt","rgb24","pipe:1"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.len(), width as usize * height as usize * 3);
    out.stdout
}

/// Replace only font references in the reference script, on BOTH sides. FATE's
/// proprietary families are not reproducible and must not leak to OS fallback.
fn fixed_fonts(source: &str) -> String {
    let mut out = String::new();
    for line in source.lines() {
        let mut line = if line.starts_with("Style:") {
            let mut fields: Vec<_> = line.split(',').collect();
            if fields.len() > 2 { fields[1] = "DejaVu Sans"; }
            fields.join(",")
        } else { line.into() };
        let mut at = 0;
        while let Some(start) = line[at..].find("\\fn").map(|i| i + at) {
            let end = line[start + 3..].find(['\\','}']).map_or(line.len(), |i| start + 3 + i);
            line.replace_range(start + 3..end,"DejaVu Sans"); at = start + 3 + "DejaVu Sans".len();
        }
        out.push_str(&line); out.push('\n');
    }
    out
}

fn check_script(name: &str, source: &str, width: u32, height: u32) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("libass-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    let source = fixed_fonts(source);
    let header: String = source.lines().filter(|s| !s.starts_with("Dialogue:") && !s.starts_with("Comment:")).map(|s| format!("{s}\n")).collect();
    let mut failures = Vec::new();
    for (i, line) in source.lines().filter(|s| s.starts_with("Dialogue:")).enumerate() {
        // Put the single event inside Events even if FATE ends in another section.
        let script = format!("{header}\n[Events]\n{line}\n");
        let path = dir.join(format!("cue-{i}.ass"));
        std::fs::write(&path,&script).unwrap();
        let mut track = Track::new(); track.process_data(script.as_bytes());
        assert_eq!(track.events.len(),1,"{name}:{i}: one source event");
        let event = track.events[0].clone();
        let mut renderer = Renderer::new(&common::options());
        // Early and late samples catch fades, moves, transformations and karaoke.
        for fraction in [0.2, 0.7] {
            let at = event.start + (event.duration as f64 * fraction) as i64;
            let got = renderer.render(&mut track,at,width,height);
            let actual = on_black(&got.image,width,height);
            let expected = oracle(&path,at,width,height);
            if let Some(error) = compare(&format!("{name}/cue-{i}@{at}"),&actual,&expected,width as usize) {
                std::fs::write(dir.join(format!("cue-{i}-{at}-actual.rgb")),&actual).unwrap();
                std::fs::write(dir.join(format!("cue-{i}-{at}-libass.rgb")),&expected).unwrap();
                failures.push(error);
            }
        }
    }
    assert!(failures.is_empty(),"{}\nimages: {}",failures.join("\n"),dir.display());
}

#[test]
fn fate_ass_per_cue_pixels() {
    let source = std::fs::read_to_string(refcheck::fate("sub/1ededcbd7b.ass")).unwrap();
    check_script("fate-ass",&source,1280,720);
}
#[test]
fn fate_ssa_per_cue_pixels() {
    let source = std::fs::read_to_string(refcheck::fate("sub/a9-misc.ssa")).unwrap();
    check_script("fate-ssa",&source,640,480);
}
#[test]
fn overrides_and_shaping_match_libass() {
    let header = "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\nScaledBorderAndShadow: yes\nKerning: yes\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,DejaVu Sans,30,&H00FFFFFF,&H0000FFFF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2,1,2,25,25,20,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
    let texts = [
        "{\\pos(320,140)\\an5}AV office שלום العربية",
        "{\\move(100,80,500,260,0,2000)\\fad(700,500)}Moving",
        "{\\pos(320,180)\\an5\\t(0,2000,\\frz30\\fscx130\\c&H44AAFF&)}Turn",
        "{\\pos(320,180)\\an5\\xbord4\\ybord2\\blur2\\xshad-4\\yshad3}Edges",
        "{\\pos(320,180)\\an5\\clip(280,0,640,360)}Clipped",
        "{\\pos(320,180)\\an5\\iclip(m 280 0 l 360 0 360 360 280 360)}Vector clip",
        "{\\an7\\pos(100,100)\\p1}m 0 0 l 100 0 b 120 20 120 60 100 80 l 0 80{\\p0}",
        "{\\pos(320,180)\\an5}{\\kf50}Ka{\\kf80}rao{\\ko60}ke",
        "{\\q0}This sentence wraps into balanced lines, rather than running beyond the margins of the video.",
        "{\\pos(320,180)\\an5}plain {\\b1}bold {\\i1}both{\\r} reset",
    ];
    let source = texts.iter().map(|s| format!("Dialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,{s}\n")).fold(header.to_owned(), |mut a,b| { a.push_str(&b); a });
    check_script("overrides",&source,640,360);
}
