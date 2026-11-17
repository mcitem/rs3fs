use super::local::{LocalFile, matches_file};
use crate::services::settings::write_settings;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(super) const DIRECTORY: &str = "sync-recovery";

#[derive(Serialize)]
struct RecoveryRecord<'a> {
    job: Uuid,
    original: &'a Path,
    archived_at: chrono::DateTime<chrono::Utc>,
    file_name: &'a std::ffi::OsStr,
}

pub(super) fn path(snapshot: &Path) -> Result<PathBuf> {
    let parent = snapshot.parent().context("同步状态缺少父目录")?;
    Ok(parent.canonicalize()?.join(DIRECTORY))
}

pub(super) async fn archive_async(
    snapshot: &Path,
    job: Uuid,
    original: &Path,
    expected: &LocalFile,
) -> Result<PathBuf> {
    let root = path(snapshot)?;
    let original = original.to_owned();
    let expected = expected.clone();
    tokio::task::spawn_blocking(move || archive(&root, job, &original, &expected))
        .await?
        .context("无法归档本地旧版本，原文件未改动")
}

pub(super) fn archive(
    root: &Path,
    job: Uuid,
    original: &Path,
    expected: &LocalFile,
) -> Result<PathBuf> {
    match fs::create_dir(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("无法创建本地同步恢复目录"),
    }
    ensure!(root.canonicalize()? == root, "本地同步恢复目录已被重定向。");

    let file_name = original.file_name().context("本地文件缺少名称")?;
    let directory = tempfile::Builder::new().prefix("file-").tempdir_in(root)?;
    let files = directory.path().join("files");
    fs::create_dir(&files)?;
    let backup = files.join(file_name);
    let mut destination = File::create_new(&backup)?;
    std::io::copy(&mut File::open(original)?, &mut destination)
        .context("无法保存本地同步恢复副本")?;
    destination.sync_all()?;
    drop(destination);
    ensure!(
        matches_file(&backup, expected)?,
        "本地文件在归档期间发生变化。"
    );
    write_settings(
        &directory.path().join("original.json"),
        &RecoveryRecord {
            job,
            original,
            archived_at: chrono::Utc::now(),
            file_name,
        },
    )?;
    Ok(directory.keep())
}
