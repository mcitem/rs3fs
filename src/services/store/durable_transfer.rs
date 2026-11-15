use super::{Entry, MIB, Store, TransferCancelled, TransferSlots};
use crate::services::{
    transfer::TransferState,
    transfer_journal::{SavedUploadItem, SavedUploadItems, TransferJournal},
};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct FileStamp {
    size: u64,
    modified: std::time::SystemTime,
}

fn file_stamp(path: &Path) -> Result<Option<FileStamp>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(meta.is_file(), "路径不再是普通文件：{}", path.display());
            Ok(Some(FileStamp {
                size: meta.len(),
                modified: meta.modified()?,
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[derive(Deserialize, Serialize)]
struct UploadPlanItem {
    key: String,
    local: Option<(PathBuf, FileStamp)>,
}

#[derive(Deserialize, Serialize)]
struct DownloadPlan {
    original: Option<FileStamp>,
}

impl Store {
    pub(super) async fn recovery_head(&self, key: &str) -> Result<Option<(String, i64)>> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;
        match result {
            Ok(output) => Ok(Some((
                output.e_tag().context("服务端未返回对象 ETag")?.to_owned(),
                output.content_length().context("服务端未返回对象长度")?,
            ))),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|error| error.is_not_found()) =>
            {
                Ok(None)
            }
            Err(error) => Err(super::connection_error(&format!("无法核对 /{key}"), error)),
        }
    }

    pub(crate) async fn journal_remote(
        &self,
        target: &Store,
        journal: &TransferJournal,
        targets: &[(Entry, String)],
        copying: bool,
        cancel: &CancellationToken,
        report: &mut (impl FnMut(String) + Send),
    ) -> Result<()> {
        report("正在读取对象清单".into());
        let saved = journal.load::<Vec<(Entry, String)>>("objects.json")?;
        let resumed = saved.is_some();
        let plan = match saved {
            Some(plan) => plan,
            None => {
                let mut plan = Vec::new();
                for (entry, destination) in targets {
                    let entries = tokio::select! {
                        _ = cancel.cancelled() => bail!(TransferCancelled),
                        result = self.plan_copy(target, entry, destination) => result?,
                    };
                    plan.extend(entries);
                }
                let mut unique = std::collections::HashSet::new();
                ensure!(
                    plan.iter().all(|(_, key)| unique.insert(key.as_str())),
                    "计划中包含相同的目标对象"
                );
                journal.save("objects.json", &plan)?;
                plan
            }
        };
        let total = plan.len();
        let mut copied = 0;
        for (index, (object, destination)) in plan.iter().enumerate() {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            let checkpoint = format!("copy-{index}.json");
            if let Some(expected) = journal.load::<String>(&checkpoint)? {
                if resumed {
                    let actual = target.recovery_head(destination).await?;
                    ensure!(
                        actual.as_ref().is_some_and(|(etag, size)| etag == &expected && *size == object.size()),
                        "已复制的目标对象发生变化：/{destination}"
                    );
                }
            } else {
                report(format!("复制对象 {copied}/{total}"));
                let pending = format!("copy-start-{index}.json");
                let interrupted = journal.load::<bool>(&pending)?.unwrap_or(false);
                let existing = if interrupted {
                    target.recovery_head(destination).await?
                } else {
                    None
                };
                let etag = if let Some((etag, size)) = existing {
                    ensure!(
                        size == object.size(),
                        "目标已存在且长度不同：/{destination}"
                    );
                    self.verify_existing_copy(target, object, destination, &etag, cancel)
                        .await?;
                    etag
                } else {
                    journal.save(&pending, &true)?;
                    match self
                        .copy_object_to(target, object, destination, cancel)
                        .await?
                    {
                        Some(etag) => etag,
                        None => {
                            let (etag, size) = target
                                .recovery_head(destination)
                                .await?
                                .context("已执行复制，但目标对象无法核对")?;
                            ensure!(
                                size == object.size(),
                                "复制后的目标长度不符：/{destination}"
                            );
                            self.verify_existing_copy(target, object, destination, &etag, cancel)
                                .await?;
                            etag
                        }
                    }
                };
                journal.save(&checkpoint, &etag)?;
            }
            copied += 1;
            report(format!("复制对象 {copied}/{total}"));
        }
        if !copying {
            let mut removed = 0;
            report(format!("清理源对象 {removed}/{total}"));
            for (index, (object, _)) in plan.iter().enumerate() {
                ensure!(!cancel.is_cancelled(), TransferCancelled);
                let checkpoint = format!("remove-{index}.json");
                if !journal.load::<bool>(&checkpoint)?.unwrap_or(false) {
                    if !resumed || self.recovery_head(object.key()).await?.is_some() {
                        self.delete_object(object).await.with_context(|| {
                            format!("目标已复制完毕，源已删除 {removed}/{total} 个对象")
                        })?;
                    }
                    journal.save(&checkpoint, &true)?;
                }
                removed += 1;
                report(format!("清理源对象 {removed}/{total}"));
            }
        }
        Ok(())
    }

    async fn verify_existing_copy(
        &self,
        target: &Store,
        object: &Entry,
        destination: &str,
        destination_etag: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if Self::is_directory_marker(object) {
            let source = self
                .recovery_head(object.key())
                .await?
                .context("源目录标记已不存在")?;
            ensure!(
                source.1 == 0 && object.etag().is_none_or(|etag| etag == source.0),
                "源目录标记已变化"
            );
            return Ok(());
        }
        let source_etag = object.etag().context("没有源对象 ETag，无法核对复制结果")?;
        let _permit = self
            .transfer_slots
            .acquire_worker(cancel, format!("核对复制结果，/{destination}"))
            .await?;
        let (original, copied) = tokio::try_join!(
            self.client
                .get_object()
                .bucket(&self.bucket)
                .key(object.key())
                .if_match(source_etag)
                .send(),
            target
                .client
                .get_object()
                .bucket(&target.bucket)
                .key(destination)
                .if_match(destination_etag)
                .send(),
        )?;
        ensure!(
            original.e_tag() == Some(source_etag)
                && copied.e_tag() == Some(destination_etag)
                && original.content_length() == Some(object.size())
                && copied.content_length() == Some(object.size())
                && original.metadata() == copied.metadata()
                && original.content_type() == copied.content_type()
                && original.content_encoding() == copied.content_encoding()
                && original.content_disposition() == copied.content_disposition()
                && original.content_language() == copied.content_language()
                && original.cache_control() == copied.cache_control(),
            "上次复制的结果无法确认，目标对象或元数据已变化：/{destination}"
        );
        if source_etag != destination_etag {
            compare_streams(
                original.body.into_async_read(),
                copied.body.into_async_read(),
                object.size().try_into()?,
                cancel,
            )
            .await
            .with_context(|| format!("上次复制的结果无法确认：/{destination}"))?;
        }
        Ok(())
    }

    pub(crate) async fn journal_upload(
        &self,
        journal: &TransferJournal,
        prefix: &str,
        paths: &[PathBuf],
        slots: &TransferSlots,
        cancel: &CancellationToken,
        visible: SavedUploadItems,
        report: &mut (impl FnMut(String) + Send),
    ) -> Result<()> {
        report("正在扫描本地文件".into());
        let plan = match journal.load::<Vec<UploadPlanItem>>("upload.json")? {
            Some(plan) => plan,
            None => {
                let plan = super::upload_plan(prefix, paths.to_vec())?
                    .into_iter()
                    .map(|(key, path)| {
                        let local = path
                            .map(|path| -> Result<_> {
                                let stamp = file_stamp(&path)?.context("上传源文件不存在")?;
                                Ok((path, stamp))
                            })
                            .transpose()?;
                        Ok(UploadPlanItem { key, local })
                    })
                    .collect::<Result<Vec<_>>>()?;
                journal.save("upload.json", &plan)?;
                plan
            }
        };
        let total = plan.len();
        let mut done = 0;
        let mut skipped = 0;
        let mut work = Vec::new();
        let mut items = Vec::new();
        for (index, item) in plan.iter().enumerate() {
            let state = if journal
                .load::<bool>(&format!("uploaded-{index}.json"))?
                .unwrap_or(false)
            {
                done += 1;
                TransferState::Complete
            } else if journal
                .load::<bool>(&format!("skipped-{index}.json"))?
                .unwrap_or(false)
            {
                skipped += 1;
                TransferState::Cancelled
            } else {
                work.push(index);
                TransferState::Queued
            };
            let size = item.local.as_ref().map_or(0, |(_, stamp)| stamp.size);
            let sent = if matches!(state, TransferState::Complete) {
                size
            } else {
                0
            };
            items.push(SavedUploadItem {
                key: item.key.clone(),
                size,
                directory: item.local.is_none(),
                counter: Arc::new(AtomicU64::new(sent)),
                state: Arc::new(Mutex::new(state)),
                cancel: cancel.child_token(),
            });
        }
        *visible.lock().unwrap() = Some(items.clone());
        report(format!("上传对象 {done}/{total}"));
        let items = &items;
        let plan = &plan;
        let mut jobs = stream::iter(work)
            .map(|index| async move {
                let item = &plan[index];
                let view = &items[index];
                let result = self
                    .journal_upload_item(journal, index, item, view, slots)
                    .await;
                let result = match result {
                    Ok(()) => journal.save(&format!("uploaded-{index}.json"), &true),
                    Err(error) if error.is::<TransferCancelled>() && !cancel.is_cancelled() => {
                        journal
                            .save(&format!("skipped-{index}.json"), &true)
                            .and(Err(error))
                    }
                    failure => failure,
                };
                let failure = result
                    .as_ref()
                    .err()
                    .filter(|error| !error.is::<TransferCancelled>())
                    .map(|error| format!("/{}：{error:#}", item.key));
                let state = TransferState::from_result(result);
                let completed = matches!(state, TransferState::Complete);
                let cancelled = matches!(state, TransferState::Cancelled);
                *view.state.lock().unwrap() = state;
                (completed, cancelled, failure)
            })
            .buffer_unordered(crate::services::settings::MAX_TRANSFER_PARALLELISM);

        let mut failures = Vec::new();
        while let Some((completed, cancelled, error)) = jobs.next().await {
            done += usize::from(completed);
            skipped += usize::from(cancelled);
            if let Some(error) = error {
                failures.push(error);
            }
            let mut message = format!("上传对象 {done}/{total}");
            if skipped > 0 {
                message.push_str(&format!("，取消 {skipped}"));
            }
            if !failures.is_empty() {
                message.push_str(&format!("，失败 {}", failures.len()));
            }
            report(message);
        }
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        ensure!(failures.is_empty(), "{}", failures.join("\n"));
        Ok(())
    }

    async fn journal_upload_item(
        &self,
        journal: &TransferJournal,
        index: usize,
        item: &UploadPlanItem,
        view: &SavedUploadItem,
        slots: &TransferSlots,
    ) -> Result<()> {
        let cancel = &view.cancel;
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        let start = || *view.state.lock().unwrap() = TransferState::Sending;
        if let Some((path, stamp)) = &item.local {
            ensure!(
                file_stamp(path)?.as_ref() == Some(stamp),
                "本地文件已变化：{}",
                path.display()
            );
            self.journal_upload_file(
                journal,
                index,
                &item.key,
                path,
                stamp.size,
                super::UploadTransfer {
                    sent: view.counter.clone(),
                    slots,
                    cancel,
                    on_start: &start,
                },
            )
            .await?;
            ensure!(
                file_stamp(path)?.as_ref() == Some(stamp),
                "上传期间本地文件发生变化：{}",
                path.display()
            );
        } else {
            let intent = format!("marker-{index}.json");
            let interrupted = journal.load::<bool>(&intent)?.unwrap_or(false);
            let existing = if interrupted {
                self.recovery_head(&item.key).await?
            } else {
                None
            };
            if let Some((_, size)) = existing {
                ensure!(size == 0, "目标目录标记发生变化：/{}", item.key);
            } else {
                journal.save(&intent, &true)?;
                self.transfer_marker(&item.key, slots, cancel, start)
                    .await?;
            }
        }
        Ok(())
    }

    async fn journal_upload_file(
        &self,
        journal: &TransferJournal,
        index: usize,
        key: &str,
        path: &Path,
        size: u64,
        transfer: super::UploadTransfer<'_>,
    ) -> Result<()> {
        let cancel = transfer.cancel;
        let intent = format!("upload-start-{index}.json");
        if journal.load::<bool>(&intent)?.unwrap_or(false)
            && let Some((etag, remote_size)) = self.recovery_head(key).await?
        {
            ensure!(
                u64::try_from(remote_size) == Ok(size),
                "目标已存在且长度不同：/{key}"
            );
            let _permit = transfer
                .slots
                .acquire_worker(cancel, format!("核对上传结果，/{key}"))
                .await?;
            (transfer.on_start)();
            let output = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .if_match(etag)
                .send()
                .await?;
            let local = tokio::fs::File::open(path).await?;
            compare_streams(output.body.into_async_read(), local, size, cancel)
                .await
                .with_context(|| format!("上次上传结果无法确认：/{key}"))?;
            return Ok(());
        }
        journal.save(&intent, &true)?;
        self.transfer_file_checkpointed(key, path, size, None, transfer, Some((journal, index)))
            .await?;
        Ok(())
    }

    #[cfg(windows)]
    pub(crate) async fn journal_clipboard(
        &self,
        journal: &TransferJournal,
        roots: &[Entry],
        slots: &TransferSlots,
        cancel: &CancellationToken,
        report: &mut (impl FnMut(String) + Send),
    ) -> Result<()> {
        #[derive(Deserialize, Serialize)]
        struct ClipboardPlan {
            selected: Vec<PathBuf>,
            folders: Vec<PathBuf>,
            files: Vec<(Entry, PathBuf)>,
        }
        report("正在读取剪贴板文件清单".into());
        let root = journal.clipboard_dir();
        let plan = match journal.load::<ClipboardPlan>("clipboard-plan.json")? {
            Some(plan) => plan,
            None => {
                let mut plan = ClipboardPlan {
                    selected: Vec::new(),
                    folders: Vec::new(),
                    files: Vec::new(),
                };
                let mut names = std::collections::HashSet::new();
                for entry in roots {
                    ensure!(!cancel.is_cancelled(), TransferCancelled);
                    let target = local_path(&root, entry.name())?;
                    ensure!(
                        names.insert(entry.name().to_lowercase()),
                        "选中的对象包含重复的 Windows 文件名"
                    );
                    plan.selected.push(target.clone());
                    if entry.directory() {
                        plan.folders.push(target.clone());
                        for child in self.list(entry.key(), false).await? {
                            let key = child
                                .key()
                                .strip_prefix(entry.key())
                                .context("S3 返回了目录外的对象")?;
                            if key.is_empty() {
                                continue;
                            }
                            let path = local_path(&target, key)?;
                            if child.key().ends_with('/') {
                                plan.folders.push(path);
                            } else {
                                plan.files.push((child, path));
                            }
                        }
                    } else {
                        plan.files.push((entry.clone(), target));
                    }
                }
                // On Windows a file and a folder cannot have the same name, even
                // when their S3 keys differ by case or by a terminal slash.
                let mut seen = std::collections::HashMap::new();
                for folder in &plan.folders {
                    seen.insert(folder.to_string_lossy().to_lowercase(), true);
                }
                for (_, file) in &plan.files {
                    ensure!(
                        seen.insert(file.to_string_lossy().to_lowercase(), false)
                            .is_none(),
                        "多个对象对应同一个 Windows 路径：{}",
                        file.display()
                    );
                }
                for (_, file) in &plan.files {
                    for parent in file.ancestors().skip(1).take_while(|path| *path != root) {
                        ensure!(
                            seen.get(&parent.to_string_lossy().to_lowercase()) != Some(&false),
                            "Windows 路径存在文件和目录重名：{}",
                            parent.display()
                        );
                    }
                }
                journal.save("clipboard-plan.json", &plan)?;
                plan
            }
        };
        for folder in &plan.folders {
            ensure!(folder.starts_with(&root), "剪贴板目录不在任务缓存中");
            tokio::fs::create_dir_all(folder).await?;
        }
        let total = plan.files.len();
        report(format!("准备剪贴板文件 0/{total}"));
        for (index, (entry, path)) in plan.files.iter().enumerate() {
            ensure!(path.starts_with(&root), "剪贴板文件不在任务缓存中");
            tokio::fs::create_dir_all(path.parent().context("缺少剪贴板目录")?).await?;
            let counter = AtomicU64::new(0);
            self.journal_download(
                journal,
                index as u64,
                entry,
                path,
                slots,
                cancel,
                &counter,
                &mut |_| {},
            )
            .await?;
            report(format!("准备剪贴板文件 {}/{total}", index + 1));
        }
        journal.save("clipboard-paths.json", &plan.selected)?;
        Ok(())
    }

    pub(crate) async fn journal_download(
        &self,
        journal: &TransferJournal,
        slot: u64,
        entry: &Entry,
        destination: &Path,
        slots: &TransferSlots,
        cancel: &CancellationToken,
        counter: &AtomicU64,
        report: &mut (impl FnMut(String) + Send),
    ) -> Result<()> {
        let etag = entry.etag().context("服务端未提供 ETag，无法安全续传")?;
        let size = entry.size().try_into().context("对象长度无效")?;
        let part = journal.part_path(destination, slot)?;
        let plan_key = format!("download-{slot}.json");
        let ready_key = format!("download-ready-{slot}.json");
        let plan = match journal.load::<DownloadPlan>(&plan_key)? {
            Some(plan) => plan,
            None => {
                ensure!(!part.exists(), "续传文件路径已被占用");
                let plan = DownloadPlan {
                    original: file_stamp(destination)?,
                };
                journal.save(&plan_key, &plan)?;
                plan
            }
        };
        if let Some(digest) = journal.load::<String>(&ready_key)? {
            if !part.exists() {
                ensure!(
                    hash_file(destination).await? == digest,
                    "无法确认上次保存的下载文件"
                );
                counter.store(size, Ordering::Relaxed);
                report(format!("已保存 {} B", size));
                return Ok(());
            }
        }
        ensure!(
            file_stamp(destination)? == plan.original,
            "本地目标文件已变化：{}",
            destination.display()
        );
        let chunk_size = 16 * MIB;
        let chunk_count = size.div_ceil(chunk_size).max(1);
        let mut remaining = Vec::new();
        let mut received = 0;
        for index in 0..chunk_count {
            let start = index * chunk_size;
            let length = (size - start).min(chunk_size);
            let key = format!("range-{slot}-{index}.json");
            if let Some(expected) = journal.load::<Option<String>>(&key)?.flatten() {
                ensure!(file_stamp(&part)?.is_some(), "之前保存的部分下载文件已丢失");
                if hash_range(&part, start, length).await? == expected {
                    received += length;
                    continue;
                }
                // A damaged range can be fetched again against the saved object ETag.
                journal.save(&key, &Option::<String>::None)?;
            }
            remaining.push((index, start, length));
        }
        let _ = file_stamp(&part)?;
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&part)
            .await?;
        file.set_len(size).await?;
        counter.store(received, Ordering::Relaxed);
        report(format!("下载字节 {received}/{size}"));
        let fetch = |(index, start, length): (u64, u64, u64), ranged: bool, checkpoint: bool| {
            let part = &part;
            async move {
                let end = start + length.saturating_sub(1);
                let _permit = slots
                    .acquire_worker(
                        cancel,
                        format!("下载，/{}，区间 {}", entry.key(), index + 1),
                    )
                    .await?;
                let output = self
                    .client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(entry.key())
                    .if_match(etag)
                    .set_range(ranged.then(|| format!("bytes={start}-{end}")))
                    .send()
                    .await
                    .context("无法读取下载范围")?;
                ensure!(output.e_tag() == Some(etag), "远端对象版本已变化");
                if ranged {
                    if output.content_range().is_none() {
                        return Ok(false);
                    }
                    ensure!(
                        output.content_range()
                            == Some(format!("bytes {start}-{end}/{size}").as_str()),
                        "服务端未返回正确的续传范围"
                    );
                }
                let mut file = tokio::fs::OpenOptions::new().write(true).open(part).await?;
                file.seek(std::io::SeekFrom::Start(start)).await?;
                let mut input = output.body.into_async_read();
                let mut buffer = vec![0; 256 * 1024];
                let mut written = 0;
                let mut hash = Sha256::new();
                loop {
                    let count = input.read(&mut buffer).await?;
                    if count == 0 {
                        break;
                    }
                    written += count as u64;
                    ensure!(written <= length, "下载范围数据超出预期");
                    file.write_all(&buffer[..count]).await?;
                    hash.update(&buffer[..count]);
                    counter.fetch_add(count as u64, Ordering::Relaxed);
                }
                ensure!(written == length, "下载范围不完整");
                file.sync_data().await?;
                if checkpoint {
                    journal.save(
                        &format!("range-{slot}-{index}.json"),
                        &Some(super::hex_digest(&hash.finalize())),
                    )?;
                }
                Ok::<_, anyhow::Error>(true)
            }
        };
        let download = async {
            if chunk_count > 1 && !remaining.is_empty() {
                let first = remaining.remove(0);
                if !fetch(first, true, true).await? {
                    // Some compatible gateways send the full object for Range.
                    // Still save it atomically, with a fresh request for the full body.
                    counter.store(0, Ordering::Relaxed);
                    fetch((0, 0, size), false, false).await?;
                    return Ok::<_, anyhow::Error>(());
                }
            }
            let fetch = &fetch;
            stream::iter(remaining)
                .map(|range| async move {
                    ensure!(
                        fetch(range, chunk_count > 1, true).await?,
                        "服务端在下载过程中停止支持范围请求"
                    );
                    Ok(())
                })
                .buffer_unordered(crate::services::settings::MAX_TRANSFER_PARALLELISM)
                .try_collect::<Vec<()>>()
                .await?;
            Ok(())
        };
        tokio::select! {
            _ = cancel.cancelled() => bail!(TransferCancelled),
            result = download => result?,
        };
        drop(file);
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        ensure!(
            file_stamp(destination)? == plan.original,
            "本地目标文件已变化：{}",
            destination.display()
        );
        let digest = hash_file(&part).await?;
        journal.save(&ready_key, &digest)?;
        std::fs::rename(&part, destination).context("无法保存下载文件")?;
        report(format!("已保存 {size} B"));
        Ok(())
    }
}

async fn compare_streams(
    mut left: impl tokio::io::AsyncRead + Unpin,
    mut right: impl tokio::io::AsyncRead + Unpin,
    mut remaining: u64,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut expected = vec![0; 256 * 1024];
    let mut actual = vec![0; expected.len()];
    while remaining > 0 {
        let count = remaining.min(expected.len() as u64) as usize;
        tokio::select! {
            _ = cancel.cancelled() => bail!(TransferCancelled),
            result = async {
                tokio::try_join!(
                    left.read_exact(&mut actual[..count]),
                    right.read_exact(&mut expected[..count]),
                )?;
                Ok::<_, std::io::Error>(())
            } => result?,
        }
        ensure!(actual[..count] == expected[..count], "对象内容不同");
        remaining -= count as u64;
    }
    Ok(())
}

async fn hash_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let len = file.read(&mut buffer).await?;
        if len == 0 {
            return Ok(super::hex_digest(&hash.finalize()));
        }
        hash.update(&buffer[..len]);
    }
}

async fn hash_range(path: &Path, start: u64, mut length: u64) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    while length > 0 {
        let count = (length as usize).min(buffer.len());
        let read = file.read(&mut buffer[..count]).await?;
        if read == 0 {
            return Ok(String::new());
        }
        hash.update(&buffer[..read]);
        length -= read as u64;
    }
    Ok(super::hex_digest(&hash.finalize()))
}

#[cfg(windows)]
pub(crate) fn local_path(root: &Path, key: &str) -> Result<PathBuf> {
    let mut path = root.to_owned();
    for name in key.trim_end_matches('/').split('/') {
        ensure!(
            !name.is_empty()
                && !name.ends_with(['.', ' '])
                && !name
                    .chars()
                    .any(|ch| ch.is_control() || "<>:\"\\|?*".contains(ch))
                && matches!(
                    Path::new(name).components().next(),
                    Some(std::path::Component::Normal(_))
                ),
            "对象名称无法保存为 Windows 文件：{name}"
        );
        let normalized = name
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_ascii_uppercase();
        ensure!(
            !matches!(
                normalized.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) && !["COM", "LPT"].into_iter().any(|prefix| normalized
                .strip_prefix(prefix)
                .is_some_and(|suffix| matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                ))),
            "对象名称是 Windows 保留名称：{name}"
        );
        path.push(name);
    }
    Ok(path)
}
