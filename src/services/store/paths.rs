use super::trash;
use anyhow::{Context, Result, ensure};
use std::{
    io,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub(crate) fn rename_noclobber(source: &Path, destination: &Path) -> io::Result<()> {
    let mut source = tempfile::TempPath::try_from_path(source)?;
    source.disable_cleanup(true);
    source
        .persist_noclobber(destination)
        .map_err(|error| error.error)
}

pub(crate) fn child_key(prefix: &str, name: &str, directory: bool) -> Result<String> {
    ensure!(
        !name.trim().is_empty()
            && !matches!(name, "." | "..")
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control),
        "名称不能为空，且不能包含斜杠、控制字符或仅由 . / .. 组成。"
    );
    let suffix = if directory { "/" } else { "" };
    let key = format!("{prefix}{name}{suffix}");
    ensure!(key.len() <= 1024, "对象键超过 1024 字节。");
    Ok(key)
}

pub(crate) fn parent_prefix(key: &str) -> &str {
    let key = key.strip_suffix('/').unwrap_or(key);
    key.rfind('/').map_or("", |index| &key[..=index])
}

pub(crate) fn upload_plan(
    prefix: &str,
    paths: Vec<PathBuf>,
) -> Result<Vec<(String, Option<PathBuf>)>> {
    let mut plan = std::collections::BTreeMap::new();
    for root in paths {
        let base = root
            .parent()
            .context("请拖入文件或文件夹，根卷没有目录名。")?;
        for item in WalkDir::new(&root)
            .follow_links(false)
            .follow_root_links(false)
        {
            let item = item?;
            let kind = item.file_type();
            ensure!(
                kind.is_file() || kind.is_dir(),
                "无法上传链接或特殊文件：{}",
                item.path().display()
            );
            let relative = item.path().strip_prefix(base)?;
            let mut key = prefix.to_string();
            let mut components = relative.iter().peekable();
            while let Some(name) = components.next() {
                key = child_key(
                    &key,
                    name.to_str().context("本地名称不是有效 UTF-8")?,
                    components.peek().is_some() || kind.is_dir(),
                )?;
            }
            // S3 infers nonempty folders from their children. Only empty folders need markers.
            if kind.is_dir()
                && std::fs::read_dir(item.path())?
                    .next()
                    .transpose()?
                    .is_some()
            {
                continue;
            }
            let path = kind.is_file().then(|| item.into_path());
            ensure!(
                plan.insert(key.clone(), path).is_none(),
                "多个本地项对应同一目标：/{key}"
            );
        }
    }
    ensure!(
        plan.keys().all(|key| !trash::reserved(key)),
        "上传路径与回收站保留路径冲突。"
    );
    Ok(plan.into_iter().collect())
}
