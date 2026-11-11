//! The real Settings window (`murmur --ui settings`) in an invisible nested KWin: it must end up
//! exactly as tall as its content, fully on screen, even when a taller state arrives while the
//! compositor is still setting the window up. That's what happens when Settings opens: the
//! system check (D-019) answers within milliseconds. Before the fix, the window kept its first
//! height and the checklist was cut off.
//!
//! Needs kwin_wayland and spectacle on the host (see `tests/harness`). Run with:
//!
//! ```sh
//! cargo test --test ui_nested_kwin -- --ignored --nocapture
//! # keep the screenshots: MURMUR_SCREENSHOTS=<dir>
//! ```

mod harness;

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use murmur::ui::{
    offscreen::{self, ShotOptions},
    protocol::{ToUi, UiState, Window},
    samples,
};

/// Screenshot size, and the first and last row with a pixel clearly brighter than KWin's
/// black background: the window's frame, decoration included.
fn window_rows(png_path: &Path) -> (u32, u32, u32) {
    let file = std::fs::File::open(png_path).expect("open screenshot");
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let channels = info.color_type.samples();
    let row_len = info.line_size;
    let lit = |y: u32| {
        let row = &buf[y as usize * row_len..(y as usize + 1) * row_len];
        row.chunks(channels)
            .any(|px| px[..3].iter().map(|&c| u32::from(c)).sum::<u32>() > 3 * 60)
    };
    let top = (0..info.height)
        .find(|&y| lit(y))
        .expect("no window on screen");
    let bottom = (0..info.height).rev().find(|&y| lit(y)).expect("no window");
    (info.height, top, bottom)
}

fn content_height(state: &UiState) -> f32 {
    let shot = offscreen::render(Window::Settings, state, &ShotOptions::default())
        .expect("offscreen render");
    shot.height as f32
}

fn send(stdin: &mut impl Write, state: &UiState) {
    let line = serde_json::to_string(&ToUi::State(state.clone())).expect("serialize");
    writeln!(stdin, "{line}").expect("write to the window");
    stdin.flush().expect("flush");
}

/// The log without terminal colour codes (`ESC [ … m`).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The window's last `fit` log line: (window height, content height), logical px.
fn last_fit(log: &str) -> (f32, f32) {
    let line = log
        .lines()
        .rev()
        .find(|l| l.contains(" fit "))
        .unwrap_or_else(|| panic!("no fit line in the window's log:\n{log}"));
    let field = |name: &str| -> f32 {
        let at = line
            .find(&format!("{name}="))
            .unwrap_or_else(|| panic!("no {name}= in {line}"))
            + name.len()
            + 1;
        line[at..]
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("bad {name} in {line}"))
    };
    (field("window"), field("content"))
}

#[test]
fn checking_doesnt_change_the_height() {
    // The window opens while the check runs; an all-good result must not resize it.
    let mut state = samples::settings();
    state.system = None;
    let checking = content_height(&state);
    state.system = samples::settings().system;
    assert_eq!(checking, content_height(&state));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs kwin_wayland and spectacle; see the file header"]
async fn settings_window_fits_a_state_that_arrives_while_opening() {
    fits_a_late_state(harness::Nested::start_scaled("us", 1280, 1024, 1.0).await).await;
}

/// The maintainer's screens: 2560×1440 at 150 %.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs kwin_wayland and spectacle; see the file header"]
async fn settings_window_fits_at_150_percent() {
    fits_a_late_state(harness::Nested::start_scaled("us", 2560, 1440, 1.5).await).await;
}

async fn fits_a_late_state(nested: harness::Nested) {
    let log_path = nested.dir.join("ui.log");
    let log = std::fs::File::create(&log_path).expect("log file");
    let mut ui = Command::new(env!("CARGO_BIN_EXE_murmur"))
        .args(["--ui", "settings"])
        .env_clear()
        .envs(nested.session_env())
        .env("WAYLAND_DISPLAY", &nested.socket)
        .env("RUST_LOG", "murmur=debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("start the window");
    let mut stdin = ui.stdin.take().expect("stdin");
    let shots = harness::ChildEnv {
        dir: nested.dir.clone(),
        socket: nested.socket.clone(),
        bus: nested.bus.clone(),
        pad: std::path::PathBuf::new(),
    };

    // Opens while the check runs, and the answer comes at once: a failing line that wraps
    // (taller than the pending checklist).
    let mut state = samples::settings();
    state.system = None;
    send(&mut stdin, &state);
    state.system = samples::settings_idle().system;
    send(&mut stdin, &state);
    let want = content_height(&state);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (screen, top, bottom) = window_rows(&shots.screenshot("settings-fitted").await);

    drop(stdin);
    let _ = ui.wait();
    let log = strip_ansi(&std::fs::read_to_string(&log_path).unwrap_or_default());
    let (window, content) = last_fit(&log);
    eprintln!(
        "window {window} px for content {content} px (want {want}); rows {top}..{bottom} of {screen}"
    );
    assert_eq!(content, want, "the window didn't get the last state");
    assert!(
        (window - content).abs() < 1.0,
        "window {window} px tall, content {content} px (cut off)"
    );
    assert!(
        top > 0 && bottom < screen - 1,
        "the window runs off the screen (rows {top}..{bottom} of {screen})"
    );
}
