//! Model downloads: stream to a `.part` file while hashing, verify size and
//! sha256, extract into a temporary directory, then rename into place. A
//! model directory therefore only ever exists complete.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::models::{self, Asset, ModelInfo};

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("network error: {0}")]
    Http(String),
    #[error("download is corrupt ({0})")]
    Corrupt(String),
    #[error("cannot write to {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("archive error: {0}")]
    Archive(String),
}

/// Progress of a model install, as a fraction of all bytes still to fetch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Progress {
    Downloading { done: u64, total: u64 },
    Unpacking,
}

/// Don't report progress more often than this.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Downloads and installs everything `model` still needs into `models_dir`.
pub async fn install(
    model: &'static ModelInfo,
    models_dir: &Path,
    mut progress: impl FnMut(Progress) + Send,
) -> Result<(), DownloadError> {
    tokio::fs::create_dir_all(models_dir)
        .await
        .map_err(|e| DownloadError::Io(models_dir.to_path_buf(), e))?;
    let missing: Vec<&Asset> = models::assets(model)
        .into_iter()
        .filter(|a| !models::asset_present(a, models_dir))
        .collect();
    let total: u64 = missing.iter().map(|a| a.bytes).sum();
    let client = reqwest::Client::builder()
        .user_agent(concat!("murmur/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| DownloadError::Http(e.to_string()))?;
    let mut done_before = 0u64;
    for asset in missing {
        let part = models_dir.join(format!(".{}.part", asset.name));
        fetch(&client, asset, &part, |d| {
            progress(Progress::Downloading {
                done: done_before + d,
                total,
            });
        })
        .await?;
        done_before += asset.bytes;
        if asset.archive {
            progress(Progress::Unpacking);
            let (dir, part2, name) = (models_dir.to_path_buf(), part.clone(), asset.name);
            tokio::task::spawn_blocking(move || unpack(&part2, &dir, name))
                .await
                .map_err(|e| DownloadError::Archive(e.to_string()))??;
            let _ = tokio::fs::remove_file(&part).await;
        } else {
            let dest = models_dir.join(asset.name);
            tokio::fs::rename(&part, &dest)
                .await
                .map_err(|e| DownloadError::Io(dest, e))?;
        }
        info!(asset = asset.name, "installed");
    }
    Ok(())
}

async fn fetch(
    client: &reqwest::Client,
    asset: &Asset,
    part: &Path,
    mut progress: impl FnMut(u64),
) -> Result<(), DownloadError> {
    let io = |e| DownloadError::Io(part.to_path_buf(), e);
    let resp = client
        .get(asset.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| DownloadError::Http(e.to_string()))?;
    let mut file = tokio::fs::File::create(part).await.map_err(io)?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    let mut last = Instant::now();
    let mut body = resp.bytes_stream();
    let result: Result<(), DownloadError> = async {
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| DownloadError::Http(e.to_string()))?;
            got += chunk.len() as u64;
            if got > asset.bytes {
                return Err(DownloadError::Corrupt("larger than expected".into()));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(io)?;
            if last.elapsed() >= PROGRESS_INTERVAL {
                last = Instant::now();
                progress(got);
            }
        }
        file.flush().await.map_err(io)?;
        if got != asset.bytes {
            return Err(DownloadError::Corrupt(format!(
                "got {got} of {} bytes",
                asset.bytes
            )));
        }
        let digest = hex(&hasher.finalize());
        if digest != asset.sha256 {
            return Err(DownloadError::Corrupt("checksum mismatch".into()));
        }
        progress(got);
        Ok(())
    }
    .await;
    if result.is_err() {
        drop(file);
        if let Err(e) = tokio::fs::remove_file(part).await {
            warn!(error = %e, "could not remove partial download");
        }
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extracts `archive` (tar.bz2) into `models_dir/name` atomically.
fn unpack(archive: &Path, models_dir: &Path, name: &str) -> Result<(), DownloadError> {
    let staging = models_dir.join(format!(".{name}.extract"));
    let io = |p: &Path| {
        let p = p.to_path_buf();
        move |e| DownloadError::Io(p, e)
    };
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(io(&staging))?;
    }
    std::fs::create_dir_all(&staging).map_err(io(&staging))?;
    let result = (|| {
        let file = std::fs::File::open(archive).map_err(io(archive))?;
        let mut tar = tar::Archive::new(bzip2::read::BzDecoder::new(std::io::BufReader::new(file)));
        // `unpack` refuses entries that would escape `staging`.
        tar.unpack(&staging)
            .map_err(|e| DownloadError::Archive(e.to_string()))?;
        let inner = staging.join(name);
        if !inner.is_dir() {
            return Err(DownloadError::Archive(format!(
                "{name}/ missing in archive"
            )));
        }
        let dest = models_dir.join(name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(io(&dest))?;
        }
        std::fs::rename(&inner, &dest).map_err(io(&dest))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Removes a model's files (not the shared VAD).
pub async fn delete(model: &'static ModelInfo, models_dir: &Path) -> Result<(), DownloadError> {
    let dir = model.dir(models_dir);
    tokio::task::spawn_blocking(move || {
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| DownloadError::Io(dir.clone(), e))
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|e| DownloadError::Archive(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_digest() {
        let d = Sha256::digest(b"abc");
        assert_eq!(
            hex(&d),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn unpack_is_atomic_and_checks_the_top_dir() {
        let tmp = std::env::temp_dir().join(format!("murmur-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src/m1")).expect("mkdir");
        std::fs::write(tmp.join("src/m1/a.txt"), b"hello").expect("write");
        // build m1.tar.bz2 containing m1/a.txt
        let archive = tmp.join("m1.tar.bz2");
        {
            let f = std::fs::File::create(&archive).expect("create");
            let enc = bzip2::write::BzEncoder::new(f, bzip2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            b.append_dir_all("m1", tmp.join("src/m1")).expect("append");
            b.into_inner().expect("tar").finish().expect("bz2");
        }
        let models = tmp.join("models");
        std::fs::create_dir_all(&models).expect("mkdir");
        unpack(&archive, &models, "m1").expect("unpack");
        assert_eq!(
            std::fs::read(models.join("m1/a.txt")).expect("read"),
            b"hello"
        );
        assert!(!models.join(".m1.extract").exists());
        // wrong top-level name → error, nothing installed
        assert!(unpack(&archive, &models, "other").is_err());
        assert!(!models.join("other").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// `cargo test --lib download::network_tests -- --ignored`
    #[tokio::test]
    #[ignore = "downloads ~31 MB from GitHub"]
    async fn installs_a_real_model() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tmp = std::env::temp_dir().join(format!("murmur-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let model = models::find("moonshine-tiny-en").expect("registered");
        let mut last = None;
        install(model, &tmp, |p| last = Some(p))
            .await
            .expect("install");
        assert!(model.is_installed(&tmp));
        let total = models::assets(model).iter().map(|a| a.bytes).sum::<u64>();
        assert_eq!(last, Some(Progress::Downloading { done: total, total }));
        assert_eq!(model.missing_bytes(&tmp), 0);
        let leftovers: Vec<_> = std::fs::read_dir(&tmp)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        delete(model, &tmp).await.expect("delete");
        assert!(!model.dir(&tmp).exists() && tmp.join(models::SILERO_VAD.name).exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
