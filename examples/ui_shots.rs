//! Renders the Setup and Settings windows offscreen to PNG files. Needs no display and never
//! opens a window.
//!
//!     cargo run --example ui_shots -- [OUT_DIR] [--scale N]
//!
//! Writes `<name>.png` (light) and `<name>-dark.png` for each sample state into OUT_DIR
//! (default `ui-shots`).

use std::{fs::File, io::BufWriter, path::PathBuf};

use anyhow::{Context, Result, bail};
use murmur::ui::{
    offscreen::{self, Shot, ShotOptions},
    protocol::{UiState, Window},
    samples,
};

fn main() -> Result<()> {
    let mut out = PathBuf::from("ui-shots");
    let mut scale = 1.0f32;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scale" => {
                scale = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .context("--scale needs a number")?;
            }
            "-h" | "--help" => bail!("usage: ui_shots [OUT_DIR] [--scale N]"),
            dir => out = PathBuf::from(dir),
        }
    }
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    let shots: [(&str, Window, UiState, bool); 8] = [
        ("setup-fresh", Window::Setup, samples::setup_fresh(), false),
        ("setup-done", Window::Setup, samples::setup_done(), false),
        (
            "setup-typing-denied",
            Window::Setup,
            samples::typing_denied(),
            false,
        ),
        (
            "setup-download-error",
            Window::Setup,
            samples::download_error(),
            false,
        ),
        (
            "setup-unsupported",
            Window::Setup,
            samples::unsupported_desktop(),
            false,
        ),
        (
            "setup-model-list",
            Window::Setup,
            samples::setup_fresh(),
            true,
        ),
        ("settings", Window::Settings, samples::settings(), false),
        (
            "settings-idle",
            Window::Settings,
            samples::settings_idle(),
            false,
        ),
    ];
    for (name, window, state, open_model_list) in shots {
        for dark in [false, true] {
            let opts = ShotOptions {
                dark,
                scale,
                open_model_list,
            };
            let shot = offscreen::render(window, &state, &opts)
                .with_context(|| format!("rendering {name}"))?;
            let file = out.join(if dark {
                format!("{name}-dark.png")
            } else {
                format!("{name}.png")
            });
            write_png(&file, &shot)?;
            println!("{} ({}x{})", file.display(), shot.width, shot.height);
        }
    }
    Ok(())
}

fn write_png(path: &std::path::Path, shot: &Shot) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), shot.width, shot.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&shot.rgb)?;
    writer.finish()?;
    Ok(())
}
