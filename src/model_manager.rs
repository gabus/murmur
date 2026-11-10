//! Which model is active, what's installed, and the (single) running download.
//! Publishes a [`ModelsView`] for the tray menu.

use std::path::PathBuf;

use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::{info, warn};

use crate::{
    config::Config,
    download::{self, DownloadError, Progress},
    models::{self, LoadSpec, ModelInfo},
};

/// Snapshot for the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelsView {
    pub active: &'static str,
    pub installed: Vec<&'static str>,
    pub download: Option<DownloadView>,
    /// The last download that failed (model id, message); cleared when a
    /// download starts or succeeds.
    pub failed: Option<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadView {
    pub model: &'static str,
    pub percent: u8,
    pub unpacking: bool,
}

#[derive(Debug)]
pub enum ModelEvent {
    Progress(&'static str, Progress),
    Finished(&'static str, Result<(), DownloadError>),
}

/// What the app should do after a selection or a finished download.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing changes for the ASR worker.
    None,
    /// Load this model from the next session on.
    Switch(LoadSpec),
    /// A download failed (message for the user).
    Failed(String),
}

pub struct ModelManager {
    config: Config,
    config_path: Option<PathBuf>,
    template: LoadSpec,
    view: watch::Sender<ModelsView>,
    task: Option<(&'static str, JoinHandle<()>)>,
    events_tx: mpsc::UnboundedSender<ModelEvent>,
}

impl ModelManager {
    /// `template` provides models_dir, threads and engine settings; its model is
    /// replaced by the configured one.
    pub fn new(
        config: Config,
        config_path: Option<PathBuf>,
        template: LoadSpec,
    ) -> (
        Self,
        watch::Receiver<ModelsView>,
        mpsc::UnboundedReceiver<ModelEvent>,
    ) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let active = models::find(&config.model).unwrap_or_else(|| {
            warn!(model = %config.model, "unknown model in config, using the default");
            template.model
        });
        let template = LoadSpec {
            model: active,
            min_silence: config.pause_secs.clamp(0.3, 3.0),
            ..template
        };
        let (view, view_rx) = watch::channel(ModelsView {
            active: active.id,
            installed: Vec::new(),
            download: None,
            failed: None,
        });
        let mut m = Self {
            config,
            config_path,
            template,
            view,
            task: None,
            events_tx,
        };
        m.refresh();
        (m, view_rx, events_rx)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Changes and saves the config. A new pause length needs the engine
    /// reloaded, which is returned as `Outcome::Switch`.
    pub fn update_config(&mut self, change: impl FnOnce(&mut Config)) -> Outcome {
        change(&mut self.config);
        self.save_config();
        let pause = self.config.pause_secs.clamp(0.3, 3.0);
        if (pause - self.template.min_silence).abs() > f32::EPSILON {
            self.template.min_silence = pause;
            return Outcome::Switch(self.template.clone());
        }
        Outcome::None
    }

    fn save_config(&self) {
        if let Some(p) = &self.config_path
            && let Err(e) = self.config.save_to(p)
        {
            warn!(error = %e, "could not save config");
        }
    }

    pub fn active(&self) -> &'static ModelInfo {
        self.template.model
    }

    pub fn active_spec(&self) -> LoadSpec {
        self.template.clone()
    }

    pub fn active_installed(&self) -> bool {
        self.active().is_installed(&self.template.models_dir)
    }

    pub fn downloading(&self) -> Option<&'static str> {
        self.task.as_ref().map(|(id, _)| *id)
    }

    /// The user picked a model in the menu.
    pub fn select(&mut self, id: &str) -> Outcome {
        let Some(model) = models::find(id) else {
            return Outcome::None;
        };
        if model.is_installed(&self.template.models_dir) {
            self.cancel();
            return self.activate(model);
        }
        if self.downloading() != Some(model.id) {
            self.start_download(model);
        }
        Outcome::None
    }

    /// Starts downloading `model` (cancels any other download).
    pub fn start_download(&mut self, model: &'static ModelInfo) {
        self.cancel();
        let dir = self.template.models_dir.clone();
        let tx = self.events_tx.clone();
        info!(
            model = model.id,
            bytes = model.missing_bytes(&dir),
            "downloading model"
        );
        let handle = tokio::spawn(async move {
            let ptx = tx.clone();
            let result = download::install(model, &dir, move |p| {
                let _ = ptx.send(ModelEvent::Progress(model.id, p));
            })
            .await;
            let _ = tx.send(ModelEvent::Finished(model.id, result));
        });
        self.task = Some((model.id, handle));
        self.view.send_modify(|v| {
            v.failed = None;
            v.download = Some(DownloadView {
                model: model.id,
                percent: 0,
                unpacking: false,
            });
        });
    }

    pub fn cancel(&mut self) {
        if let Some((id, h)) = self.task.take() {
            h.abort();
            info!(model = id, "download cancelled");
            self.refresh();
        }
    }

    /// Deletes a downloaded model that isn't active.
    pub async fn delete(&mut self, id: &str) -> Result<(), String> {
        let Some(model) = models::find(id) else {
            return Ok(());
        };
        if model.id == self.active().id {
            return Err("can't delete the model in use".into());
        }
        if self.downloading() == Some(model.id) {
            self.cancel();
        }
        let r = download::delete(model, &self.template.models_dir)
            .await
            .map_err(|e| e.to_string());
        self.refresh();
        r
    }

    pub fn on_event(&mut self, ev: ModelEvent) -> Outcome {
        match ev {
            ModelEvent::Progress(id, p) => {
                if self.downloading() == Some(id) {
                    let (percent, unpacking) = match p {
                        Progress::Downloading { done, total } => {
                            ((done * 100 / total.max(1)).min(100) as u8, false)
                        }
                        Progress::Unpacking => (100, true),
                    };
                    self.view.send_modify(|v| {
                        v.download = Some(DownloadView {
                            model: id,
                            percent,
                            unpacking,
                        });
                    });
                }
                Outcome::None
            }
            ModelEvent::Finished(id, result) => {
                if self.downloading() != Some(id) {
                    return Outcome::None; // cancelled meanwhile
                }
                self.task = None;
                self.refresh();
                self.view.send_modify(|v| {
                    v.failed = result.as_ref().err().map(|e| (id, e.to_string()));
                });
                match result {
                    Ok(()) => match models::find(id) {
                        Some(m) => {
                            info!(model = id, "download complete");
                            self.activate(m)
                        }
                        None => Outcome::None,
                    },
                    Err(e) => {
                        warn!(model = id, error = %e, "download failed");
                        Outcome::Failed(e.to_string())
                    }
                }
            }
        }
    }

    fn activate(&mut self, model: &'static ModelInfo) -> Outcome {
        if model.id == self.active().id {
            self.refresh();
            return Outcome::None;
        }
        self.template.model = model;
        self.config.model = model.id.to_owned();
        self.save_config();
        info!(model = model.id, "model selected");
        self.refresh();
        Outcome::Switch(self.template.clone())
    }

    /// Re-reads what's installed and republishes the view.
    pub fn refresh(&mut self) {
        let dir = &self.template.models_dir;
        let installed = models::MODELS
            .iter()
            .filter(|m| m.is_installed(dir))
            .map(|m| m.id)
            .collect();
        let active = self.template.model.id;
        let downloading = self.downloading();
        self.view.send_modify(|v| {
            v.active = active;
            v.installed = installed;
            if downloading.is_none() {
                v.download = None;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::engine::EngineConfig;

    fn fake_install(dir: &std::path::Path, id: &str) {
        let m = models::find(id).expect("registered");
        std::fs::create_dir_all(m.dir(dir)).expect("mkdir");
        for f in m.files {
            std::fs::write(m.dir(dir).join(f), b"x").expect("write");
        }
        std::fs::write(dir.join(models::SILERO_VAD.name), b"x").expect("write");
    }

    fn manager(tag: &str, config: Config) -> (ModelManager, watch::Receiver<ModelsView>, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("murmur-mm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("mkdir");
        fake_install(&tmp, "parakeet-v3");
        fake_install(&tmp, "moonshine-tiny-en");
        let template = LoadSpec {
            model: models::find(models::DEFAULT_MODEL).expect("default"),
            models_dir: tmp.clone(),
            threads: 1,
            engine: EngineConfig::default(),
            min_silence: 0.8,
        };
        let (m, view, _events) = ModelManager::new(config, Some(tmp.join("config.toml")), template);
        (m, view, tmp)
    }

    #[tokio::test]
    async fn selecting_an_installed_model_switches_and_saves() {
        let (mut m, view, tmp) = manager("switch", Config::default());
        assert_eq!(view.borrow().active, "parakeet-v3");
        assert_eq!(
            view.borrow().installed,
            vec!["parakeet-v3", "moonshine-tiny-en"]
        );
        let Outcome::Switch(spec) = m.select("moonshine-tiny-en") else {
            panic!("expected a switch");
        };
        assert_eq!(spec.model.id, "moonshine-tiny-en");
        assert_eq!(view.borrow().active, "moonshine-tiny-en");
        let saved = Config::load_from(&tmp.join("config.toml"));
        assert_eq!(saved.model, "moonshine-tiny-en");
        // selecting the active model again changes nothing
        assert!(matches!(m.select("moonshine-tiny-en"), Outcome::None));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn the_active_model_cannot_be_deleted() {
        let (mut m, view, tmp) = manager("delete", Config::default());
        assert!(m.delete("parakeet-v3").await.is_err());
        m.delete("moonshine-tiny-en").await.expect("delete");
        assert_eq!(view.borrow().installed, vec!["parakeet-v3"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn unknown_model_in_config_falls_back_to_default() {
        let cfg = Config {
            model: "no-such-model".into(),
            ..Config::default()
        };
        let (m, _view, tmp) = manager("unknown", cfg);
        assert_eq!(m.active().id, models::DEFAULT_MODEL);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn finished_events_for_cancelled_downloads_are_ignored() {
        let (mut m, _view, tmp) = manager("cancelled", Config::default());
        let out = m.on_event(ModelEvent::Finished("moonshine-base-en", Ok(())));
        assert!(matches!(out, Outcome::None));
        assert_eq!(m.active().id, models::DEFAULT_MODEL);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
