//! Dev tool: dump tray icon pixmaps as raw ARGB for previewing.
//! `cargo run --example icons_preview -- <out-dir>`
use murmur::icons::{LEVELS, Look, render};

fn main() -> std::io::Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let mut looks: Vec<(String, Look)> = vec![("idle".into(), Look::Idle)];
    looks.extend((0..LEVELS).map(|l| (format!("listen{l}"), Look::Listening { level: l })));
    looks.extend((0..4).map(|p| (format!("load{p}"), Look::Loading { phase: p })));
    looks.push(("error".into(), Look::Error));
    for size in [22u32, 48] {
        for (name, look) in &looks {
            let p = render(*look, size);
            std::fs::write(format!("{dir}/{name}_{size}.argb"), &p.data)?;
        }
    }
    Ok(())
}
