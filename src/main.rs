use anyhow::{Context, Result};
use murmur::{
    app::{App, Command, Status},
    asr::{
        engine::EngineConfig,
        worker::{AsrWorker, WorkerConfig},
    },
    audio::Level,
    hotkey::Hotkey,
    output::{EisSource, Output},
    paths,
};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Stage 1: hard-coded default model (the model registry comes in stage 3).
const PARAKEET_DIR: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
const VAD_FILE: &str = "silero_vad.onnx";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(run())
}

async fn run() -> Result<()> {
    let models = paths::models_dir().context("cannot determine $XDG_DATA_HOME")?;
    let state = paths::state_dir().context("cannot determine $XDG_STATE_HOME")?;
    std::fs::create_dir_all(&state).context("creating state dir")?;

    if !ashpd::is_sandboxed() {
        // Host apps must tell the portal who they are (needs the .desktop file installed).
        let id = ashpd::AppID::try_from(paths::APP_ID).context("app id")?;
        if let Err(e) = ashpd::register_host_app(id).await {
            warn!(error = %e, "could not register host app ID with the portal");
        }
    }

    let threads = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 4))
        .unwrap_or(2) as i32;
    let (asr_tx, asr_rx) = mpsc::unbounded_channel();
    let asr = AsrWorker::spawn(
        WorkerConfig {
            parakeet_dir: models.join(PARAKEET_DIR),
            vad_model: models.join(VAD_FILE),
            min_silence: 0.8,
            threads,
            engine: EngineConfig::default(),
        },
        asr_tx,
    )
    .context("starting ASR worker")?;

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let source = EisSource::Portal {
        token_path: state.join("remote-desktop.token"),
    };
    #[cfg(feature = "dev-harness")]
    let source = if std::env::var("MURMUR_DEV_EIS").as_deref() == Ok("kwin-direct") {
        EisSource::KwinDirect
    } else {
        source
    };
    let output = Output::spawn(source, out_tx);

    let (hk_tx, hk_rx) = mpsc::unbounded_channel();
    let use_hotkey =
        !cfg!(feature = "dev-harness") || std::env::var_os("MURMUR_DEV_NO_HOTKEY").is_none();
    let _hotkey = if use_hotkey {
        Some(
            Hotkey::start(hk_tx)
                .await
                .context("binding the global shortcut")?,
        )
    } else {
        None
    };

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    #[cfg(feature = "dev-harness")]
    if std::env::var_os("MURMUR_DEV_AUTORUN").is_some() {
        // Start listening now; quit (which finishes the phrase) when the WAV ends.
        let _ = cmd_tx.send(Command::Toggle);
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            murmur::dev::EOF.notified().await;
            let _ = tx.send(Command::Quit);
        });
    }
    let (status_tx, mut status_rx) = watch::channel(Status::Idle);
    tokio::spawn(async move {
        while status_rx.changed().await.is_ok() {
            info!(status = ?*status_rx.borrow(), "status");
        }
    });
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = cmd_tx.send(Command::Quit);
    });

    info!("murmur ready — press the shortcut to dictate");
    App::new(asr, output, Level::default(), status_tx)
        .run(cmd_rx, hk_rx, asr_rx, out_rx)
        .await;
    Ok(())
}
