//! Native-route smoke on macOS: plays a URL (or file) through `Player` and
//! `AppleBackend` in a non-activating panel, muted, prints the state as it
//! changes and runs a script of steps, then exits.
//!
//! `cargo run -p player --example native_play -- <url> [steps...]`
//!
//! Steps: `wait:<s>`, `seek:<s>`, `pause`, `play`, `audio:<stream|none>`,
//! `sub:<stream|none>`, `shot:<png path>` (the panel, via `screencapture`),
//! `end:<timeout s>` (until the playback ends or fails). Default:
//! `wait:5 end:120`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use player::{AppleBackend, Player, PlayerOptions, State};

fn describe(state: &State) -> String {
    format!(
        "pos {:7.3} dur {:?} playing {} buffering {} ended {} error {:?} video {:?} audio {:?} sub {:?}",
        state.position.as_secs_f64(),
        state.duration.map(|d| (d.as_secs_f64() * 1000.0).round() / 1000.0),
        state.playing,
        state.buffering,
        state.ended,
        state.error,
        state.video_decoder,
        state.audio,
        state.subtitle,
    )
}

fn main() {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSPanel, NSWindowStyleMask};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    let mut args = std::env::args().skip(1);
    let url = args.next().expect("native_play <url> [steps...]");
    let mut steps: Vec<String> = args.collect();
    if steps.is_empty() {
        steps = vec!["wait:5".into(), "end:120".into()];
    }

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let size = CGSize { width: 640.0, height: 360.0 };
    let window = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        CGRect { origin: CGPoint { x: 40.0, y: 40.0 }, size },
        NSWindowStyleMask::Titled | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    window.setTitle(&objc2_foundation::NSString::from_str("native_play"));
    window.setFloatingPanel(true);
    window.setHidesOnDeactivate(false);
    let window_number = window.windowNumber();
    let view = window.contentView().expect("content view");
    let backend = AppleBackend::new();
    backend.set_muted(true);
    // SAFETY: `view` is a live NSView retained by the panel for the
    // process's life.
    unsafe { backend.attach(std::ptr::from_ref(&*view).cast_mut().cast()) };
    backend.set_frame([0.0, 0.0, size.width, size.height]);
    window.orderFrontRegardless();

    std::thread::spawn(move || {
        let started = Instant::now();
        let stamp = move || format!("{:7.2}", started.elapsed().as_secs_f64());
        let player = Player::open(&url, backend, Arc::new(codecs::context()), PlayerOptions::default(), |_| {});
        let mut last = String::new();
        let mut watch = |player: &Player, until: Instant| -> State {
            loop {
                let state = player.state();
                let line = describe(&state);
                // Positions move every tick: print when something else
                // changed, or once a second.
                let key = line.split_once(" dur").map_or(line.clone(), |(_, rest)| rest.to_string());
                if key != last || started.elapsed().as_millis() % 1000 < 100 {
                    println!("{} {line}", stamp());
                    last = key;
                }
                if Instant::now() >= until || state.ended || state.error.is_some() {
                    return state;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        for step in &steps {
            let (verb, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
            let secs = || arg.parse::<f64>().expect("seconds");
            let stream = || if arg == "none" { None } else { Some(arg.parse::<u32>().expect("stream index")) };
            println!("{} STEP {step}", stamp());
            match verb {
                "wait" => {
                    watch(&player, Instant::now() + Duration::from_secs_f64(secs()));
                }
                "seek" => player.seek(Duration::from_secs_f64(secs())),
                "pause" => player.pause(),
                "play" => player.play(),
                "audio" => player.select_audio(stream()),
                "sub" => player.select_subtitle(stream()),
                "shot" => {
                    let status = std::process::Command::new("screencapture")
                        .args(["-x", "-o", &format!("-l{window_number}"), arg])
                        .status();
                    println!("{} shot {arg}: {status:?}", stamp());
                }
                "end" => {
                    let state = watch(&player, Instant::now() + Duration::from_secs_f64(secs()));
                    println!("{} END ended {} error {:?}", stamp(), state.ended, state.error);
                }
                other => panic!("unknown step {other}"),
            }
        }
        println!("{} done: {}", stamp(), describe(&player.state()));
        drop(player);
        println!("{} dropped", stamp());
        std::process::exit(0);
    });
    app.run();
}
