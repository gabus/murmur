//! Model registry: what can be downloaded, where it lives, how to load it.
//!
//! Files live in `$XDG_DATA_HOME/murmur/models/<archive dir>/`, using the
//! archive's own top-level directory name. A model counts as installed when
//! all its required files exist (extraction is atomic, see `download.rs`).

use std::path::{Path, PathBuf};

use crate::{
    asr::{
        AsrError, Engine,
        engine::{EngineConfig, RecheckEngine, Transcriber},
        light::LightEngine,
        moonshine::Moonshine,
        parakeet::Parakeet,
    },
    vad::SileroVad,
};

/// A downloadable file: a `.tar.bz2` that unpacks into `dir`, or a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// Directory (archives) or file name (single files) under the models dir.
    pub name: &'static str,
    pub archive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Parakeet,
    Moonshine,
    /// Streaming Zipformer + punctuation model (`extras[0]`).
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Stable id stored in the config file.
    pub id: &'static str,
    pub name: &'static str,
    /// One line for menus: strengths and languages.
    pub summary: &'static str,
    pub kind: Kind,
    pub asset: Asset,
    /// Files inside `asset.name` that must exist.
    pub files: &'static [&'static str],
    /// Further archives the model needs (e.g. a punctuation model).
    pub extras: &'static [Asset],
}

/// Shared by every model: end-of-phrase detection.
pub const SILERO_VAD: Asset = Asset {
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
    bytes: 643_854,
    sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
    name: "silero_vad.onnx",
    archive: false,
};

pub const DEFAULT_MODEL: &str = "parakeet-v3";

pub static MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "parakeet-v3",
        name: "Parakeet v3",
        summary: "Most accurate · 25 European languages",
        kind: Kind::Parakeet,
        asset: Asset {
            url: concat!(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/",
                "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"
            ),
            bytes: 487_211_443,
            sha256: "1495e91b59d7fc53ddca91f3431758ec99f8fa253fdb0e1e851be0f72b14b0f9",
            name: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
            archive: true,
        },
        files: &[
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "joiner.int8.onnx",
            "tokens.txt",
        ],
        extras: &[],
    },
    ModelInfo {
        id: "moonshine-base-en",
        name: "Moonshine base",
        summary: "Lighter · English",
        kind: Kind::Moonshine,
        asset: Asset {
            url: concat!(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/",
                "sherpa-onnx-moonshine-base-en-quantized-2026-02-27.tar.bz2"
            ),
            bytes: 111_266_225,
            sha256: "43232c1d13013d37317163baec3135bd771a186a4356f28c889bab453bb0e891",
            name: "sherpa-onnx-moonshine-base-en-quantized-2026-02-27",
            archive: true,
        },
        files: &[
            "encoder_model.ort",
            "decoder_model_merged.ort",
            "tokens.txt",
        ],
        extras: &[],
    },
    ModelInfo {
        id: "moonshine-tiny-en",
        name: "Moonshine tiny",
        summary: "Lightest · English",
        kind: Kind::Moonshine,
        asset: Asset {
            url: concat!(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/",
                "sherpa-onnx-moonshine-tiny-en-quantized-2026-02-27.tar.bz2"
            ),
            bytes: 29_858_559,
            sha256: "9ec31b342d8fa3240c3b81b8f82e1cf7e3ac467c93ca5a999b741d5887164f8d",
            name: "sherpa-onnx-moonshine-tiny-en-quantized-2026-02-27",
            archive: true,
        },
        files: &[
            "encoder_model.ort",
            "decoder_model_merged.ort",
            "tokens.txt",
        ],
        extras: &[],
    },
    ModelInfo {
        id: "zipformer-light-en",
        name: "Light",
        summary: "Lowest CPU · English · streaming",
        kind: Kind::Light,
        asset: Asset {
            url: concat!(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/",
                "sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2"
            ),
            bytes: 310_414_022,
            sha256: "639e25b578e9e997131402199419c13a941f8e4e198e2da1ce57dbf5cf401282",
            name: "sherpa-onnx-streaming-zipformer-en-2023-06-26",
            archive: true,
        },
        // Order matters for the light engine: encoder, decoder, joiner, tokens.
        files: &[
            "encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx",
            "decoder-epoch-99-avg-1-chunk-16-left-128.onnx",
            "joiner-epoch-99-avg-1-chunk-16-left-128.int8.onnx",
            "tokens.txt",
        ],
        extras: &[PUNCT_EN],
    },
];

/// English punctuation + casing for the light engine.
pub const PUNCT_EN: Asset = Asset {
    url: concat!(
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/punctuation-models/",
        "sherpa-onnx-online-punct-en-2024-08-06.tar.bz2"
    ),
    bytes: 30_667_839,
    sha256: "9f5e5a72c7d2829635bd074fce92b6bbd5b78da8a52e7ad8ed1be933f366b99d",
    name: "sherpa-onnx-online-punct-en-2024-08-06",
    archive: true,
};

/// Looks a model up by id.
pub fn find(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

/// Assets a model needs (the model itself plus shared ones).
pub fn assets(model: &ModelInfo) -> Vec<&Asset> {
    let mut v = vec![&model.asset];
    v.extend(model.extras.iter());
    v.push(&SILERO_VAD);
    v
}

impl ModelInfo {
    /// Directory holding this model's files.
    pub fn dir(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(self.asset.name)
    }

    pub fn is_installed(&self, models_dir: &Path) -> bool {
        let dir = self.dir(models_dir);
        self.files.iter().all(|f| dir.join(f).is_file())
            && self.extras.iter().all(|a| asset_present(a, models_dir))
            && models_dir.join(SILERO_VAD.name).is_file()
    }

    /// Download size of what's still missing.
    pub fn missing_bytes(&self, models_dir: &Path) -> u64 {
        assets(self)
            .into_iter()
            .filter(|a| !asset_present(a, models_dir))
            .map(|a| a.bytes)
            .sum()
    }

    /// Human-readable download size, e.g. "490 MB".
    pub fn size_label(&self) -> String {
        human_size(self.asset.bytes)
    }
}

pub fn asset_present(asset: &Asset, models_dir: &Path) -> bool {
    let p = models_dir.join(asset.name);
    if asset.archive {
        p.is_dir()
    } else {
        p.is_file()
    }
}

pub fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 100.0 {
        format!("{:.0} MB", (mb / 10.0).round() * 10.0)
    } else {
        format!("{mb:.0} MB")
    }
}

/// Everything needed to load a model on the ASR worker.
#[derive(Debug, Clone)]
pub struct LoadSpec {
    pub model: &'static ModelInfo,
    pub models_dir: PathBuf,
    pub threads: i32,
    pub engine: EngineConfig,
    /// Pause that ends a phrase (seconds).
    pub min_silence: f32,
}

/// Loads the model and VAD into a ready engine (blocking, can take seconds).
pub fn load_engine(spec: &LoadSpec) -> Result<Box<dyn Engine>, AsrError> {
    let vad = SileroVad::load(&spec.models_dir.join(SILERO_VAD.name), spec.min_silence)?;
    let dir = spec.model.dir(&spec.models_dir);
    let transcriber: Box<dyn Transcriber + Send> = match spec.model.kind {
        Kind::Parakeet => Box::new(Parakeet::load(&dir, spec.threads)?),
        Kind::Moonshine => Box::new(Moonshine::load(&dir, spec.threads)?),
        Kind::Light => {
            let punct = spec
                .model
                .extras
                .first()
                .map(|a| spec.models_dir.join(a.name))
                .ok_or_else(|| AsrError::UnknownModel(spec.model.id.into()))?;
            let [encoder, decoder, joiner, tokens] = spec.model.files else {
                return Err(AsrError::UnknownModel(spec.model.id.into()));
            };
            return Ok(Box::new(LightEngine::load(
                &dir,
                [encoder, decoder, joiner, tokens],
                &punct,
                vad,
                spec.threads.min(2),
                spec.engine.light_commit_delay,
            )?));
        }
    };
    Ok(Box::new(RecheckEngine::new(
        spec.engine.clone(),
        transcriber,
        vad,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASES: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download";

    #[test]
    fn registry_is_consistent() {
        assert!(find(DEFAULT_MODEL).is_some());
        let mut ids: Vec<_> = MODELS.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MODELS.len(), "duplicate ids");
        for m in MODELS {
            assert!(m.asset.url.starts_with(RELEASES), "{}", m.id);
            assert!(m.asset.url.ends_with(".tar.bz2") && m.asset.archive);
            for e in m.extras {
                assert!(e.url.starts_with(RELEASES) && e.url.contains(e.name));
            }
            assert!(m.asset.url.contains(m.asset.name), "{}", m.id);
            assert_eq!(m.asset.sha256.len(), 64);
            assert!(!m.files.is_empty());
        }
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(human_size(487_211_443), "490 MB");
        assert_eq!(human_size(29_858_559), "30 MB");
    }

    #[test]
    fn installed_needs_all_files_and_vad() {
        let tmp = std::env::temp_dir().join(format!("murmur-models-{}", std::process::id()));
        let m = find("moonshine-tiny-en").expect("registered");
        let dir = m.dir(&tmp);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(!m.is_installed(&tmp));
        for f in m.files {
            std::fs::write(dir.join(f), b"x").expect("write");
        }
        assert!(!m.is_installed(&tmp), "VAD still missing");
        assert_eq!(m.missing_bytes(&tmp), SILERO_VAD.bytes);
        std::fs::write(tmp.join(SILERO_VAD.name), b"x").expect("write");
        assert!(m.is_installed(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
