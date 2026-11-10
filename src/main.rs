use anyhow::{Context, Result};
use murmur::{
    app::{App, Channels, Command, Parts, Status},
    asr::{engine::EngineConfig, worker::AsrWorker},
    audio::Level,
    hotkey::Hotkey,
    instance::{self, Instance},
    notify::Notifier,
    output::{EisSource, Output},
    paths,
};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--ui") {
        // The UI process; never start a second daemon from here (it would ask
        // the running one to open a window, which would start another UI…).
        anyhow::bail!("this build has no UI windows yet");
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // One murmur per session: a second start opens the running one's settings.
    let _bus = match instance::acquire(cmd_tx.clone()).await {
        Ok(Instance::Primary(conn)) => Some(conn),
        Ok(Instance::AlreadyRunning) => return Ok(()),
        Err(e) => {
            warn!(error = %e, "single-instance check unavailable");
            None
        }
    };

    let models = paths::models_dir().context("cannot determine $XDG_DATA_HOME")?;
    let state = paths::state_dir().context("cannot determine $XDG_STATE_HOME")?;
    std::fs::create_dir_all(&state).context("creating state dir")?;

    // Before any portal call (needs the .desktop file installed when not sandboxed).
    murmur::portal::register_host_app().await;
    let _ = rustls::crypto::ring::default_provider().install_default();

    let threads = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 4))
        .unwrap_or(2) as i32;
    let config_path = murmur::config::Config::path();
    let config = config_path
        .as_deref()
        .map(murmur::config::Config::load_from)
        .unwrap_or_default();
    let first_run = !config.setup_done;
    let paste_keys = if config.paste_keys.eq_ignore_ascii_case("ctrl+shift+v") {
        murmur::output::PasteKeys::CtrlShiftV
    } else {
        murmur::output::PasteKeys::CtrlV
    };
    let default_model = murmur::models::find(murmur::models::DEFAULT_MODEL)
        .context("default model missing from the registry")?;
    let (models_mgr, models_view, model_events) = murmur::model_manager::ModelManager::new(
        config,
        config_path,
        murmur::models::LoadSpec {
            model: default_model,
            models_dir: models.clone(),
            min_silence: 0.8,
            threads,
            engine: EngineConfig::default(),
        },
    );
    let (asr_tx, asr_rx) = mpsc::unbounded_channel();
    let asr = AsrWorker::spawn(models_mgr.active_spec(), asr_tx).context("starting ASR worker")?;

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
    // On first run the typing-permission dialog waits for "Allow…" in the setup page.
    let output = if first_run {
        Output::spawn_idle(source, out_tx)
    } else {
        Output::spawn(source, out_tx)
    };

    output.set_paste_keys(paste_keys);

    let (hk_tx, hk_rx) = mpsc::unbounded_channel();
    let use_hotkey =
        !cfg!(feature = "dev-harness") || std::env::var_os("MURMUR_DEV_NO_HOTKEY").is_none();
    let hotkey = use_hotkey.then(|| Hotkey::spawn(hk_tx));

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
    let (shortcut_tx, shortcut_rx) = watch::channel(None);
    let (level, level_rx) = Level::new();
    let tray_status = status_rx.clone();
    tokio::spawn(async move {
        while status_rx.changed().await.is_ok() {
            info!(status = ?*status_rx.borrow(), "status");
        }
    });
    tokio::spawn(murmur::tray::run(
        tray_status,
        level_rx,
        models_view.clone(),
        cmd_tx.clone(),
        shortcut_rx,
    ));
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = cmd_tx.send(Command::Quit);
    });

    info!("murmur ready — press the shortcut to dictate");
    let view = models_view.borrow().clone();
    App::new(
        Parts {
            asr,
            output,
            hotkey,
            level,
            status: status_tx,
            shortcut_label: shortcut_tx,
            notifier: Notifier::spawn(),
            models: models_mgr,
        },
        view,
    )
    .run(Channels {
        commands: cmd_rx,
        hotkey: hk_rx,
        asr: asr_rx,
        output: out_rx,
        models: model_events,
        models_view,
    })
    .await;
    Ok(())
}
