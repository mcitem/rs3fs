use super::{Entry, Store, TransferCancelled, connection_error};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_types::{DateTime, date_time::Format};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default)]
enum TrashPrefix {
    #[default]
    Current,
    Legacy,
}

impl TrashPrefix {
    const ALL: [Self; 2] = [Self::Current, Self::Legacy];

    fn root(self) -> &'static str {
        match self {
            Self::Current => ".rs3fs-trash",
            Self::Legacy => ".s3fs-trash",
        }
    }

    fn manifests(self) -> String {
        format!("{}/entries/", self.root())
    }
}

pub(super) fn reserved(key: &str) -> bool {
    TrashPrefix::ALL.iter().any(|prefix| {
        key.strip_prefix(prefix.root())
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('/'))
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TrashEntry {
    id: Uuid,
    original: String,
    directory: bool,
    deleted_at: i64,
    objects: Vec<TrashObject>,
    pending: bool,
    interrupted: bool,
    #[serde(default)]
    archive_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    marker_only: bool,
    // The manifest's object key determines where its copies live.
    #[serde(skip)]
    prefix: TrashPrefix,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TrashObject {
    key: String,
    etag: String,
}

impl TrashEntry {
    pub(crate) fn id(&self) -> Uuid {
        self.id
    }

    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    pub(crate) fn interrupted(&self) -> bool {
        self.interrupted
    }

    pub(crate) fn can_restore(&self) -> bool {
        self.copy_verified != Some(false)
            && (!self.archive_only || !(self.pending || self.interrupted))
            && !(self.marker_only && self.pending)
    }

    pub(crate) fn expired(&self, cutoff: i64) -> bool {
        !self.pending && !self.interrupted && self.deleted_at <= cutoff
    }

    pub(crate) fn deleted_at(&self) -> String {
        DateTime::from_secs(self.deleted_at)
            .fmt(Format::DateTime)
            .unwrap_or_default()
            .replace('T', " ")
    }

    pub(crate) fn manifest(&self) -> String {
        format!("{}{}.json", self.prefix.manifests(), self.id)
    }

    fn data_prefix(&self) -> String {
        format!("{}/objects/{}/", self.prefix.root(), self.id)
    }
}

impl Store {
    pub(crate) async fn trash(&self, entry: Entry) -> Result<()> {
        self.trash_after_copy(entry, &CancellationToken::new(), || Ok(()))
            .await
    }

    pub(crate) async fn trash_after_copy(
        &self,
        entry: Entry,
        cancel: &CancellationToken,
        before_remove: impl FnOnce() -> Result<()> + Send,
    ) -> Result<()> {
        self.save_trash(entry, cancel, Some(before_remove)).await
    }

    pub(crate) async fn archive_in_trash(
        &self,
        entry: Entry,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.save_trash(entry, cancel, None::<fn() -> Result<()>>)
            .await
    }

    async fn save_trash(
        &self,
        entry: Entry,
        cancel: &CancellationToken,
        before_remove: Option<impl FnOnce() -> Result<()> + Send>,
    ) -> Result<()> {
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        ensure!(
            !entry.key.is_empty() && !reserved(&entry.key),
            "无法删除此路径。"
        );
        let objects = self.objects(&entry).await?;
        let marker_only = before_remove.is_some()
            && entry.directory
            && matches!(objects.as_slice(), [object]
                if object.key == entry.key && Self::is_directory_marker(object));
        for object in &objects {
            ensure!(
                object.etag.is_some() || Self::is_directory_marker(object),
                "S3 未返回对象 ETag：/{}",
                object.key
            );
        }
        let mut record = TrashEntry {
            id: Uuid::new_v4(),
            original: entry.key,
            directory: entry.directory,
            deleted_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
            objects: objects
                .iter()
                .map(|object| TrashObject {
                    key: object.key.clone(),
                    etag: object.etag.clone().unwrap_or_default(),
                })
                .collect(),
            pending: true,
            interrupted: false,
            archive_only: before_remove.is_none(),
            copy_verified: (!entry.directory).then_some(false),
            marker_only,
            prefix: TrashPrefix::Current,
        };
        // Publish the plan before creating copies so every partial operation remains visible.
        self.put(
            &record.manifest(),
            ByteStream::from(serde_json::to_vec(&record)?),
        )
        .await?;
        let result = self
            .copy_to_trash(&mut record, &objects, cancel, before_remove)
            .await;
        record.pending = false;
        record.interrupted = result.is_err();
        self.write_trash_manifest(&record).await?;
        result
    }

    async fn write_trash_manifest(&self, record: &TrashEntry) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(record.manifest())
            .body(ByteStream::from(serde_json::to_vec(&record)?))
            .send()
            .await
            .map_err(|error| connection_error("无法更新回收记录", error))?;
        Ok(())
    }

    async fn copy_to_trash(
        &self,
        record: &mut TrashEntry,
        objects: &[Entry],
        cancel: &CancellationToken,
        before_remove: Option<impl FnOnce() -> Result<()> + Send>,
    ) -> Result<()> {
        if record.marker_only {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            let before_remove = before_remove.context("回收空目录需要删除原目录标记。")?;
            before_remove().context("已保存回收记录，原目录标记尚未删除。")?;
            return self.delete_object(&objects[0]).await;
        }
        let prefix = record.data_prefix();
        for (index, object) in objects.iter().enumerate() {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            let key = format!("{prefix}{index}");
            let copied_etag = self
                .copy_object_to(self, object, &key, cancel)
                .await
                .context("移入回收站未完成，源对象尚未删除。")?;
            let mut backup = self.recovery_head(&key).await?;
            if record.copy_verified.is_some() && !Self::is_directory_marker(object) {
                // Some compatible endpoints acknowledge the copy before its HEAD
                // metadata is visible. Keep the original pinned throughout the retry.
                for delay in [200, 700] {
                    if archive_matches(backup.as_ref(), copied_etag.as_deref(), object.size()) {
                        break;
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(TransferCancelled.into()),
                        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    }
                    backup = self.recovery_head(&key).await?;
                }
                if !archive_matches(backup.as_ref(), copied_etag.as_deref(), object.size()) {
                    let details = format!(
                        "S3 回收副本校验不一致：预期 {} 字节，复制 ETag {:?}，查询结果 {:?}",
                        object.size(),
                        copied_etag,
                        backup
                    );
                    // The manifest owns this unique backup key. Delete only the
                    // observed version of this copy and create its replacement conditionally.
                    ensure!(!cancel.is_cancelled(), TransferCancelled);
                    if let Some((etag, _)) = &backup {
                        self.delete_if_unchanged(&key, etag)
                            .await
                            .with_context(|| details.clone())?;
                    }
                    self.copy_via_local_file(
                        self,
                        object,
                        &key,
                        object.etag().context("S3 未返回源对象 ETag")?,
                        cancel,
                    )
                    .await
                    .with_context(|| format!("{details}；临时中转归档失败，源对象尚未删除"))?;
                }
                continue;
            }
            ensure!(
                backup.is_some_and(|(etag, bytes)| {
                    bytes == object.size() && copied_etag.is_none_or(|expected| expected == etag)
                }),
                "回收站副本校验失败，源对象尚未删除。"
            );
        }
        if record.copy_verified.is_some() {
            record.copy_verified = Some(true);
        }
        let Some(before_remove) = before_remove else {
            return Ok(());
        };
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        if record.copy_verified.is_some() {
            // Store the checked backup state before the original can disappear.
            self.write_trash_manifest(record)
                .await
                .context("无法保存已验证的回收记录，源对象尚未删除")?;
        }
        before_remove().context("已保存回收副本，源对象尚未删除。")?;
        for (index, object) in objects.iter().enumerate() {
            self.delete_object(object).await.with_context(|| {
                format!(
                    "源已删除 {index}/{} 项，所有副本均已保存在回收站。",
                    objects.len()
                )
            })?;
        }
        Ok(())
    }

    pub(crate) async fn list_trash(&self) -> Result<Vec<TrashEntry>> {
        let mut records = Vec::new();
        for prefix in TrashPrefix::ALL {
            records.extend(self.list_trash_in(prefix).await?);
        }
        records.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(records)
    }

    async fn list_trash_in(&self, prefix: TrashPrefix) -> Result<Vec<TrashEntry>> {
        let manifests = prefix.manifests();
        let mut records = Vec::new();
        for object in self.list_objects(&manifests, false).await? {
            let Some(id) = object
                .key
                .strip_prefix(&manifests)
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|id| Uuid::parse_str(id).ok())
            else {
                continue;
            };
            let cancel = tokio_util::sync::CancellationToken::new();
            let _permit = self
                .transfer_slots
                .acquire_download_worker(&cancel, format!("读取回收记录，/{}", object.key))
                .await?;
            let result = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&object.key)
                .send()
                .await;
            let output = match result {
                Ok(output) => output,
                Err(error) if error.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
                    continue;
                }
                Err(error) => return Err(connection_error("无法读取回收记录", error)),
            };
            let mut record: TrashEntry =
                serde_json::from_slice(&output.body.collect().await?.into_bytes())
                    .with_context(|| format!("回收记录损坏：{id}"))?;
            record.prefix = prefix;
            ensure!(
                record.id == id && record.manifest() == object.key,
                "回收记录 ID 不匹配：{id}"
            );
            records.push(record);
        }
        Ok(records)
    }

    pub(crate) async fn restore_trash(&self, record: &TrashEntry) -> Result<()> {
        ensure!(
            !(record.marker_only && record.pending),
            "空目录正在移入回收站。"
        );
        ensure!(record.can_restore(), "同步归档尚未校验完成。");
        ensure!(
            !record.original.is_empty()
                && !reserved(&record.original)
                && (!record.directory || record.original.ends_with('/'))
                && record.objects.iter().all(|object| {
                    let key = &object.key;
                    if record.directory {
                        key.starts_with(&record.original)
                    } else {
                        key == &record.original
                    }
                }),
            "回收记录包含无效路径。"
        );
        if record.marker_only {
            ensure!(
                record.directory
                    && !record.archive_only
                    && matches!(record.objects.as_slice(), [object] if object.key == record.original),
                "空目录回收记录无效。"
            );
            let key = &record.original;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .if_none_match("*")
                .body(ByteStream::from_static(b""))
                .send()
                .await
                .map_err(|error| {
                    if error
                        .raw_response()
                        .is_some_and(|response| response.status().as_u16() == 412)
                    {
                        anyhow::anyhow!("原路径已有同名对象：/{key}，回收记录已保留。")
                    } else {
                        connection_error(&format!("无法重建空目录 /{key}，回收记录已保留"), error)
                    }
                })?;
            return self.remove_trash_manifest(record).await;
        }
        let prefix = record.data_prefix();
        let objects: HashMap<_, _> = self
            .list_objects(&prefix, false)
            .await?
            .into_iter()
            .map(|object| (object.key.clone(), object))
            .collect();
        let mut copies = Vec::new();
        for (index, source) in record.objects.iter().enumerate() {
            if let Some(object) = objects.get(&format!("{prefix}{index}")) {
                copies.push((source, object));
            }
        }
        ensure!(copies.len() == objects.len(), "回收站包含无法识别的副本。");
        // Check known conflicts before starting a directory restore. The copy request
        // also checks the destination atomically against concurrent writes.
        let mut plan = Vec::new();
        for (source, copy) in copies {
            let key = &source.key;
            let result = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await;
            match result {
                Ok(output)
                    if (record.pending || record.interrupted)
                        && output.e_tag() == Some(source.etag.as_str()) =>
                {
                    plan.push((key, copy, false));
                }
                Ok(_) => anyhow::bail!("原路径已有同名对象：/{key}"),
                Err(error) if error.as_service_error().is_some_and(|e| e.is_not_found()) => {
                    plan.push((key, copy, true));
                }
                Err(error) => {
                    return Err(connection_error(&format!("无法检查原路径 /{key}"), error));
                }
            }
        }
        for (key, object, needs_copy) in plan {
            if needs_copy {
                self.copy_object(object, key).await?;
            }
            if !record.pending {
                self.delete_object(object)
                    .await
                    .with_context(|| format!("已还原 /{key}，但回收站副本清理失败。"))?;
            }
        }
        if !record.pending {
            self.remove_trash_manifest(record).await?;
        }
        Ok(())
    }

    pub(crate) async fn purge_trash(&self, record: &TrashEntry) -> Result<()> {
        ensure!(!record.pending, "回收记录尚未完成，副本仍可还原。");
        for object in self.list_objects(&record.data_prefix(), false).await? {
            self.delete_object(&object).await?;
        }
        self.remove_trash_manifest(record).await
    }

    async fn remove_trash_manifest(&self, record: &TrashEntry) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(record.manifest())
            .send()
            .await
            .map_err(|error| connection_error("无法删除回收记录", error))?;
        Ok(())
    }
}

fn archive_matches(backup: Option<&(String, i64)>, expected: Option<&str>, length: i64) -> bool {
    backup
        .zip(expected)
        .is_some_and(|((etag, bytes), expected)| {
            *bytes == length && etag.trim_matches('"') == expected.trim_matches('"')
        })
}
