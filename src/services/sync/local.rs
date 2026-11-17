use crate::services::{store::TransferCancelled, transfer_journal::is_download_partial};
use anyhow::{Context, Result, ensure};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LocalFile {
    pub(super) bytes: u64,
    pub(super) digest: [u8; 16],
}
pub(super) fn is_sync_temp_file(name: &str) -> bool {
    name.starts_with("~$") || is_download_partial(name)
}

pub(super) fn valid_component(part: &str) -> bool {
    if part.is_empty()
        || matches!(part, "." | "..")
        || part.ends_with([' ', '.'])
        || part.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
        || part.chars().any(char::is_control)
    {
        return false;
    }
    let stem = part
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

/// Windows reports file sharing conflicts and byte-range locks separately from permission errors.
#[cfg(windows)]
pub(super) fn is_busy(error: &anyhow::Error) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| {
                matches!(code as u32, ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
            })
    })
}

#[cfg(not(windows))]
pub(super) fn is_busy(_: &anyhow::Error) -> bool {
    false
}

pub(super) fn local_file(path: &Path) -> Result<LocalFile> {
    local_file_with_cancel(path, None)
}

pub(super) fn local_file_with_cancel(
    path: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<LocalFile> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !is_redirect(&metadata),
        "路径不是普通文件：{}",
        path.display()
    );
    let mut file = std::fs::File::open(path)?;
    let mut hash = Md5::new();
    let mut buffer = [0; 64 * 1024];
    let mut bytes = 0;
    loop {
        ensure!(
            !cancel.is_some_and(CancellationToken::is_cancelled),
            TransferCancelled
        );
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    if bytes != metadata.len() {
        return Err(anyhow::Error::new(super::SyncItemChanged)
            .context(format!("扫描时文件发生变化：{}", path.display())));
    }
    Ok(LocalFile {
        bytes,
        digest: hash.finalize().into(),
    })
}

pub(super) fn is_redirect(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

#[derive(Default)]
pub(super) struct LocalTree {
    pub(super) files: BTreeMap<String, LocalFile>,
    pub(super) busy: BTreeSet<String>,
    pub(super) unreadable: BTreeMap<String, String>,
    pub(super) directories: BTreeSet<String>,
}

#[cfg(test)]
pub(super) fn scan_local(root: &Path) -> Result<LocalTree> {
    scan_local_excluding(root, None, None)
}

pub(super) fn under_reserved(path: &Path, reserved: &Path) -> bool {
    let mut components = path.components();
    reserved.components().all(|expected| {
        components.next().is_some_and(|actual| {
            actual == expected
                || (cfg!(windows)
                    && actual.as_os_str().to_string_lossy().to_lowercase()
                        == expected.as_os_str().to_string_lossy().to_lowercase())
        })
    })
}

pub(super) fn recovery_relative(root: &Path, recovery: &Path) -> Result<Option<PathBuf>> {
    let absolute_root = root.canonicalize()?;
    ensure!(
        !under_reserved(&absolute_root, recovery),
        "同步目录不能位于同步恢复目录中。"
    );
    let relative = under_reserved(recovery, &absolute_root).then(|| {
        recovery
            .components()
            .skip(absolute_root.components().count())
            .collect()
    });
    Ok(relative)
}

pub(super) fn scan_local_excluding(
    root: &Path,
    excluded: Option<&Path>,
    cancel: Option<&CancellationToken>,
) -> Result<LocalTree> {
    let metadata = std::fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !is_redirect(&metadata),
        "同步目录必须是普通目录。"
    );
    let mut tree = LocalTree::default();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            excluded.is_none_or(|path| {
                entry
                    .path()
                    .strip_prefix(root)
                    .map_or(true, |relative| !under_reserved(relative, path))
            })
        })
    {
        ensure!(
            !cancel.is_some_and(CancellationToken::is_cancelled),
            TransferCancelled
        );
        let entry = entry?;
        if entry.depth() == 0 {
            continue;
        }
        if !entry.file_type().is_dir() && is_sync_temp_file(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path().strip_prefix(root)?;
        let parts = path
            .iter()
            .map(|part| {
                part.to_str()
                    .filter(|part| valid_component(part))
                    .with_context(|| format!("无法同步此路径：{}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        let key = parts.join("/");
        if entry.file_type().is_dir() {
            let metadata = std::fs::symlink_metadata(entry.path())?;
            ensure!(
                !is_redirect(&metadata),
                "同步目录包含链接或重解析点：{}",
                entry.path().display()
            );
            tree.directories.insert(key);
        } else {
            match local_file_with_cancel(entry.path(), cancel) {
                Ok(file) => {
                    tree.files.insert(key, file);
                }
                Err(error) if is_busy(&error) => {
                    tree.busy.insert(key);
                }
                Err(error) if error.is::<TransferCancelled>() => return Err(error),
                Err(error) => {
                    tree.unreadable
                        .insert(key, format!("本地文件读取失败：{error:#}"));
                }
            }
        }
    }
    Ok(tree)
}

pub(super) fn safe_target(root: &Path, relative: &str, directory: bool) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !is_redirect(&metadata),
        "同步目录已被替换。"
    );
    let mut parts = relative.split('/').peekable();
    let mut path = root.to_owned();
    while let Some(part) = parts.next() {
        ensure!(
            valid_component(part),
            "对象键无法映射为本地路径：{relative}"
        );
        path.push(part);
        if parts.peek().is_none() && !directory {
            break;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !is_redirect(&metadata),
                "目标目录不可用：{}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = std::fs::create_dir(&path) {
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                    let metadata = std::fs::symlink_metadata(&path)?;
                    ensure!(
                        metadata.is_dir() && !is_redirect(&metadata),
                        "目标目录不可用：{}",
                        path.display()
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

pub(super) fn is_missing(root: &Path, relative: &str) -> Result<bool> {
    Ok(metadata_at(root, relative)?.is_none())
}

pub(super) fn directory_exists(root: &Path, relative: &str) -> Result<bool> {
    Ok(metadata_at(root, relative)?
        .is_some_and(|metadata| metadata.is_dir() && !is_redirect(&metadata)))
}

pub(super) fn metadata_at(root: &Path, relative: &str) -> Result<Option<std::fs::Metadata>> {
    let metadata = std::fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !is_redirect(&metadata),
        "同步目录已被替换。"
    );
    let mut path = root.to_owned();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        ensure!(valid_component(part), "无效的同步路径：{relative}");
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if parts.peek().is_some() => ensure!(
                metadata.is_dir() && !is_redirect(&metadata),
                "目标目录不可用：{}",
                path.display()
            ),
            Ok(metadata) => return Ok(Some(metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

pub(super) fn matches_file(path: &Path, expected: &LocalFile) -> Result<bool> {
    matches_file_with_cancel(path, expected, None)
}

fn matches_file_with_cancel(
    path: &Path,
    expected: &LocalFile,
    cancel: Option<&CancellationToken>,
) -> Result<bool> {
    match local_file_with_cancel(path, cancel) {
        Ok(actual) => Ok(&actual == expected),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn matches_at_with_cancel(
    root: &Path,
    relative: &str,
    expected: &LocalFile,
    cancel: &CancellationToken,
) -> Result<bool> {
    if is_missing(root, relative)? {
        return Ok(false);
    }
    matches_file_with_cancel(&root.join(relative), expected, Some(cancel))
}
