use super::SyncJob;
use crate::services::settings::{ConnectionSettings, write_settings};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) fn config(name: &str) -> Result<PathBuf> {
    Ok(ConnectionSettings::path()?.with_file_name(name))
}

pub(super) fn read<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn load_jobs() -> Result<Vec<SyncJob>> {
    read_config("sync-jobs.json")
}

pub(crate) fn read_config<T: serde::de::DeserializeOwned + Default>(name: &str) -> Result<T> {
    let path = config(name)?;
    read(&path).with_context(|| format!("无法读取 {}", path.display()))
}

pub(crate) fn save_jobs(jobs: &[SyncJob]) -> Result<()> {
    load_jobs()?;
    write_settings(&config("sync-jobs.json")?, &jobs)
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Scope {
    folder: PathBuf,
    bucket: String,
    bucket_region: Option<String>,
    prefix: String,
    endpoint: String,
    region: String,
    profile: String,
}

impl Scope {
    pub(super) fn new(job: &SyncJob, connection: &ConnectionSettings) -> Result<Self> {
        Ok(Self {
            folder: job.folder.canonicalize().context("无法识别本地同步目录")?,
            bucket: job.bucket.clone(),
            bucket_region: job.bucket_region.clone(),
            prefix: job.prefix.clone(),
            endpoint: connection.endpoint().to_owned(),
            region: connection.region().to_owned(),
            profile: connection.profile().to_owned(),
        })
    }

    pub(super) fn verify(&self, snapshot: &Path) -> Result<bool> {
        let path = scope_path(snapshot)?;
        let existing: Option<Self> = read(&path)?;
        match existing {
            Some(existing) => {
                anyhow::ensure!(
                    &existing == self,
                    "同步目录或连接目标已改变。请为新目标建立同步任务，避免沿用旧基线。"
                );
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub(super) fn bind(&self, snapshot: &Path) -> Result<()> {
        if !self.verify(snapshot)? {
            write_settings(&scope_path(snapshot)?, &Some(self))?;
        }
        Ok(())
    }
}

fn scope_path(snapshot: &Path) -> Result<PathBuf> {
    let stem = snapshot.file_stem().context("同步状态路径无效")?;
    let mut name = stem.to_os_string();
    name.push(".scope.json");
    Ok(snapshot.with_file_name(name))
}
