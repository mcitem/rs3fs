use super::SyncJob;
use crate::services::settings::{ConnectionSettings, write_settings};
use anyhow::{Context, Result};
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
