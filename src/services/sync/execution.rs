use super::{
    Action, ArchiveDeferred, Direction, PlanItem, SyncItemChanged, SyncJob,
    baseline::Change,
    local,
    plan::{Pair, RemoteFile},
    recovery,
};
use crate::services::store::{Entry, Store, TransferCancelled, TransferSlots, UploadTransfer};
use anyhow::{Context, Result, ensure};
use md5::{Digest, Md5};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(super) struct ItemExecutor<'a> {
    pub(super) store: &'a Store,
    pub(super) job: &'a SyncJob,
    pub(super) snapshot: &'a Path,
    pub(super) slots: &'a TransferSlots,
    pub(super) cancel: &'a CancellationToken,
    pub(super) reviewed_uploads: bool,
}

pub(super) struct ItemResult {
    pub(super) change: Result<Option<Change>>,
    pub(super) local_archives: usize,
    pub(super) remote_archives: usize,
}

impl ItemExecutor<'_> {
    pub(super) async fn execute(&self, item: &PlanItem, action: Action) -> ItemResult {
        let Self {
            store,
            job,
            snapshot,
            slots,
            cancel,
            reviewed_uploads,
        } = *self;
        let mut local_archives = 0;
        let mut remote_archives = 0;
        let mut change = None;
        let key = item.path.as_str();
        let current_local = item.local.as_ref();
        let current_remote = item.remote.as_ref();
        let object = format!("{}{key}", job.prefix);
        let result = async {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            match action {
                Action::Upload => {
                    let fresh = job.direction == Direction::Upload && !reviewed_uploads;
                    let (staging, file) = if fresh {
                        stage_latest_upload(&job.folder, key, cancel).await?
                    } else {
                        let file = current_local.context("本地文件已不存在")?;
                        ensure!(
                            local_matches(&job.folder, key, file, cancel).await?,
                            SyncItemChanged
                        );
                        let root = job.folder.clone();
                        let key = key.to_owned();
                        let hash_cancel = cancel.clone();
                        let (staging, actual) = tokio::task::spawn_blocking(move || {
                            stage_upload(&root, &key, &hash_cancel)
                        })
                        .await??;
                        ensure!(&actual == file, SyncItemChanged);
                        (staging, actual)
                    };
                    if let Some(remote) = current_remote {
                        store
                            .archive_in_trash(
                                Entry::file(object.clone(), remote.bytes, remote.etag.clone()),
                                cancel,
                            )
                            .await
                            .map_err(|error| -> anyhow::Error {
                                if cancel.is_cancelled() {
                                    TransferCancelled.into()
                                } else {
                                    ArchiveDeferred(error).into()
                                }
                            })?;
                        remote_archives += 1;
                        ensure!(!cancel.is_cancelled(), TransferCancelled);
                        if !fresh {
                            ensure!(
                                local_matches(&job.folder, key, &file, cancel).await?,
                                SyncItemChanged
                            );
                        }
                    }
                    let etag = store
                        .transfer_file(
                            &object,
                            staging.path(),
                            file.bytes,
                            current_remote.map(|remote| remote.etag.as_str()),
                            UploadTransfer {
                                slots,
                                cancel,
                                sent: Arc::new(AtomicU64::new(0)),
                                on_start: &|| {},
                            },
                        )
                        .await?
                        .context("S3 写入响应缺少 ETag，无法记录同步状态")?;
                    change = Some(Change::Set(
                        key.to_owned(),
                        Pair {
                            local: file.clone(),
                            remote: RemoteFile {
                                bytes: i64::try_from(file.bytes)?,
                                etag,
                            },
                        },
                    ));
                }
                Action::Download => {
                    let path = local::safe_target(&job.folder, key, false)?;
                    let file = current_remote.context("远端文件已不存在")?;
                    let staging = tempfile::Builder::new()
                        .prefix(super::DOWNLOAD_STAGING_PREFIX)
                        .tempdir_in(path.parent().context("缺少目标父目录")?)?;
                    let staged = staging.path().join("download");
                    store
                        .download_with_progress(
                            &Entry::file(object.clone(), file.bytes, file.etag.clone()),
                            &staged,
                            &AtomicU64::new(0),
                            slots,
                            cancel,
                        )
                        .await?;
                    let input = staged.clone();
                    let hash_cancel = cancel.clone();
                    let downloaded = tokio::task::spawn_blocking(move || {
                        local::local_file_with_cancel(&input, Some(&hash_cancel))
                    })
                    .await??;
                    ensure!(
                        i64::try_from(downloaded.bytes)? == file.bytes,
                        "下载大小与清单不一致：{key}"
                    );
                    let archive = if let Some(existing) = current_local {
                        ensure!(
                            local_matches(&job.folder, key, existing, cancel).await?,
                            SyncItemChanged
                        );
                        let archive =
                            recovery::archive_async(snapshot, job.id, &path, existing).await?;
                        local_archives += 1;
                        Some(archive)
                    } else {
                        None
                    };
                    ensure!(
                        store
                            .recovery_head(&object)
                            .await?
                            .is_some_and(|(etag, bytes)| {
                                etag == file.etag && bytes == file.bytes
                            }),
                        SyncItemChanged
                    );
                    ensure!(!cancel.is_cancelled(), TransferCancelled);
                    if let Some((existing, archive)) = current_local.zip(archive) {
                        ensure!(
                            local_matches(&job.folder, key, existing, cancel).await?,
                            SyncItemChanged
                        );
                        std::fs::rename(&staged, &path).with_context(|| {
                            format!("无法替换本地文件，副本已保存在 {}", archive.display())
                        })?;
                    } else {
                        ensure!(local::is_missing(&job.folder, key)?, SyncItemChanged);
                        if let Err(error) = crate::services::store::rename_noclobber(&staged, &path) {
                            ensure!(
                                error.kind() != std::io::ErrorKind::AlreadyExists,
                                SyncItemChanged
                            );
                            return Err(error.into());
                        }
                    }
                    change = Some(Change::Set(
                        key.to_owned(),
                        Pair {
                            local: downloaded,
                            remote: file.clone(),
                        },
                    ));
                }
                Action::Match => {
                    let file = current_local.context("本地摘要缺失")?;
                    ensure!(
                        local_matches(&job.folder, key, file, cancel).await?,
                        SyncItemChanged
                    );
                    ensure!(
                        store
                            .recovery_head(&object)
                            .await?
                            .is_some_and(|(etag, bytes)| {
                                current_remote.is_some_and(|remote| {
                                    remote.etag == etag && remote.bytes == bytes
                                })
                            }),
                        SyncItemChanged
                    );
                    change = Some(Change::Set(
                        key.to_owned(),
                        Pair {
                            local: file.clone(),
                            remote: current_remote.context("S3 摘要缺失")?.clone(),
                        },
                    ));
                }
                Action::CreateLocalDirectory => {
                    let marker = current_remote.context("S3 目录标记摘要缺失")?;
                    ensure!(
                        store
                            .recovery_head(&format!("{object}/"))
                            .await?
                            .is_some_and(
                                |(etag, bytes)| marker.etag == etag && marker.bytes == bytes
                            ),
                        SyncItemChanged
                    );
                    local::safe_target(&job.folder, key, true)?;
                }
                Action::CreateRemoteDirectory => {
                    ensure!(local::directory_exists(&job.folder, key)?, SyncItemChanged);
                    store.mkdir(format!("{object}/")).await?;
                }
                Action::Busy | Action::Retry | Action::Conflict => {}
                Action::Keep => unreachable!(),
            }
            Ok(change)
        }
        .await;
        ItemResult {
            change: result,
            local_archives,
            remote_archives,
        }
    }
}

async fn stage_latest_upload(
    root: &Path,
    key: &str,
    cancel: &CancellationToken,
) -> Result<(tempfile::NamedTempFile, local::LocalFile)> {
    let mut retries = [Duration::from_millis(100), Duration::from_millis(250)].into_iter();
    loop {
        let root = root.to_owned();
        let key = key.to_owned();
        let copy_cancel = cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            let (staging, file) = stage_upload(&root, &key, &copy_cancel)?;
            ensure!(
                local::matches_at_with_cancel(&root, &key, &file, &copy_cancel)?,
                SyncItemChanged
            );
            Ok((staging, file))
        })
        .await?;
        match result {
            Err(error) if error.is::<SyncItemChanged>() || local::is_busy(&error) => {
                let Some(delay) = retries.next() else {
                    return if error.is::<SyncItemChanged>() {
                        Err(error.context("未能取得稳定的本地副本，暂缓上传"))
                    } else {
                        Err(error)
                    };
                };
                tokio::select! {
                    _ = cancel.cancelled() => return Err(TransferCancelled.into()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            result => return result,
        }
    }
}

fn stage_upload(
    root: &Path,
    key: &str,
    cancel: &CancellationToken,
) -> Result<(tempfile::NamedTempFile, local::LocalFile)> {
    ensure!(!cancel.is_cancelled(), TransferCancelled);
    let metadata = local::metadata_at(root, key)?.ok_or(SyncItemChanged)?;
    ensure!(
        metadata.is_file() && !local::is_redirect(&metadata),
        "本地源路径已变为非普通文件"
    );
    let mut source = match std::fs::File::open(root.join(key)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SyncItemChanged.into());
        }
        result => result?,
    };
    let mut staging = tempfile::NamedTempFile::new()?;
    let mut hash = Md5::new();
    let mut buffer = [0; 64 * 1024];
    let mut remaining = metadata.len();
    loop {
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        remaining = remaining.checked_sub(count as u64).ok_or(SyncItemChanged)?;
        staging.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
    }
    ensure!(remaining == 0, SyncItemChanged);
    Ok((
        staging,
        local::LocalFile {
            bytes: metadata.len(),
            digest: hash.finalize().into(),
        },
    ))
}

async fn local_matches(
    root: &Path,
    key: &str,
    expected: &local::LocalFile,
    cancel: &CancellationToken,
) -> Result<bool> {
    let root = root.to_owned();
    let key = key.to_owned();
    let expected = expected.clone();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        local::matches_at_with_cancel(&root, &key, &expected, &cancel)
    })
    .await?
}

pub(super) fn stage(action: Action) -> &'static str {
    match action {
        Action::Upload => "上传文件",
        Action::Download => "下载文件",
        Action::Conflict => "核对同名文件",
        Action::Match => "记录相同文件",
        Action::CreateLocalDirectory => "创建本地目录",
        Action::CreateRemoteDirectory => "创建 S3 目录",
        Action::Busy | Action::Retry => "记录待重试文件",
        Action::Keep => unreachable!(),
    }
}
