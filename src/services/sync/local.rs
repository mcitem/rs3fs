use anyhow::{Context, Result, ensure};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
    path::{Path, PathBuf},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LocalFile {
    pub(super) bytes: u64,
    pub(super) digest: [u8; 16],
}
pub(super) fn is_office_temp_file(name: &str) -> bool {
    name.starts_with("~$")
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

pub(super) fn local_file(path: &Path) -> Result<LocalFile> {
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
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    ensure!(
        bytes == metadata.len(),
        "扫描时文件发生变化：{}",
        path.display()
    );
    Ok(LocalFile {
        bytes,
        digest: hash.finalize().into(),
    })
}

fn is_redirect(metadata: &std::fs::Metadata) -> bool {
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
    pub(super) directories: BTreeSet<String>,
}

pub(super) fn scan_local(root: &Path) -> Result<LocalTree> {
    let metadata = std::fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !is_redirect(&metadata),
        "同步目录必须是普通目录。"
    );
    let mut tree = LocalTree::default();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.depth() == 0 {
            continue;
        }
        if !entry.file_type().is_dir() && is_office_temp_file(&entry.file_name().to_string_lossy())
        {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        ensure!(
            !is_redirect(&metadata),
            "同步目录包含链接或重解析点：{}",
            entry.path().display()
        );
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
        if metadata.is_dir() {
            tree.directories.insert(key);
        } else {
            tree.files.insert(key, local_file(entry.path())?);
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
                std::fs::create_dir(&path)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}
