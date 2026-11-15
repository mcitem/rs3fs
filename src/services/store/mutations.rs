#[cfg(test)]
use super::TransferCancelled;
use super::{COPY_LIMIT, Entry, Store, connection_error, trash};
#[cfg(test)]
use anyhow::anyhow;
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::primitives::ByteStream;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelocateProgress {
    Copying { done: usize, total: usize },
    Removing { done: usize, total: usize },
}

impl Store {
    pub(super) async fn objects(&self, entry: &Entry) -> Result<Vec<Entry>> {
        if entry.directory {
            let mut objects = self.list(&entry.key, false).await?;
            // Some gateways omit the prefix's own marker from recursive listings.
            if !objects.iter().any(|object| object.key == entry.key) {
                let response = self
                    .client
                    .head_object()
                    .bucket(&self.bucket)
                    .key(&entry.key)
                    .send()
                    .await;
                match response {
                    Ok(marker) => objects.push(Entry {
                        key: entry.key.clone(),
                        directory: false,
                        size: marker
                            .content_length()
                            .context("服务器未返回目录标记大小")?,
                        etag: marker.e_tag().map(str::to_owned),
                        modified: None,
                        storage_class: None,
                    }),
                    Err(error) if error.as_service_error().is_some_and(|e| e.is_not_found()) => {}
                    // A virtual directory can be operated on even if the gateway
                    // refuses HEAD for the nonexistent marker.
                    Err(_) if !objects.is_empty() => {}
                    Err(error) => {
                        return Err(connection_error(
                            &format!("无法读取目录标记 /{}", entry.key),
                            error,
                        ));
                    }
                }
            }
            ensure!(!objects.is_empty(), "源文件夹已不存在：/{}", entry.key);
            Ok(objects)
        } else {
            Ok(vec![entry.clone()])
        }
    }

    pub(crate) async fn delete_permanently(&self, entry: Entry) -> Result<()> {
        ensure!(
            !entry.key.is_empty() && !trash::reserved(&entry.key),
            "无法删除此路径。"
        );
        let objects = self.objects(&entry).await?;
        for object in &objects {
            ensure!(
                object.etag.is_some() || Self::is_directory_marker(object),
                "S3 未返回对象 ETag：/{}",
                object.key
            );
        }
        for (index, object) in objects.iter().enumerate() {
            self.delete_object(object).await.with_context(|| {
                format!("永久删除中断，已删除 {index}/{} 个对象", objects.len())
            })?;
        }
        Ok(())
    }

    pub(super) fn is_directory_marker(entry: &Entry) -> bool {
        !entry.directory && entry.size == 0 && entry.key.ends_with('/')
    }

    async fn verify_marker(&self, entry: &Entry) -> Result<Option<String>> {
        let marker = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&entry.key)
            .send()
            .await
            .map_err(|error| {
                connection_error(&format!("无法读取目录标记 /{}", entry.key), error)
            })?;
        ensure!(
            marker.content_length() == Some(0),
            "源目录标记已变化：/{}",
            entry.key
        );
        if let Some(etag) = entry.etag() {
            ensure!(
                marker.e_tag() == Some(etag),
                "源目录标记已变化：/{}",
                entry.key
            );
        }
        Ok(marker.e_tag().map(str::to_owned))
    }

    pub(super) async fn delete_object(&self, entry: &Entry) -> Result<()> {
        if let Some(etag) = entry.etag() {
            return self.delete_if_unchanged(&entry.key, etag).await;
        }
        ensure!(
            Self::is_directory_marker(entry),
            "S3 未返回对象 ETag：/{}",
            entry.key
        );
        let etag = self.verify_marker(entry).await?;
        self.delete_with_etag(&entry.key, etag.as_deref()).await
    }

    pub(crate) async fn delete_if_unchanged(&self, key: &str, etag: &str) -> Result<()> {
        self.delete_with_etag(key, Some(etag)).await
    }

    async fn delete_with_etag(&self, key: &str, etag: Option<&str>) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .set_if_match(etag.map(str::to_owned))
            .send()
            .await
            .map_err(|error| connection_error(&format!("无法删除 /{key}"), error))?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn relocate(&self, entry: Entry, destination: String) -> Result<()> {
        self.relocate_to(self, entry, destination, &CancellationToken::new())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn relocate_to(
        &self,
        target: &Store,
        entry: Entry,
        destination: String,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.relocate_to_with_progress(target, entry, destination, cancel, |_| {})
            .await
    }

    #[cfg(test)]
    pub(crate) async fn relocate_to_with_progress(
        &self,
        target: &Store,
        entry: Entry,
        destination: String,
        cancel: &CancellationToken,
        mut report: impl FnMut(RelocateProgress) + Send,
    ) -> Result<()> {
        let objects = self
            .copy_entries(target, &entry, &destination, cancel, |done, total| {
                report(RelocateProgress::Copying { done, total });
            })
            .await?;
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        let total = objects.len();
        report(RelocateProgress::Removing { done: 0, total });
        for (index, object) in objects.iter().enumerate() {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            self.delete_object(object)
                .await
                .with_context(|| format!("目标已复制完毕，源已删除 {index}/{total} 个对象"))?;
            report(RelocateProgress::Removing {
                done: index + 1,
                total,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn copy(&self, entry: Entry, destination: String) -> Result<()> {
        self.copy_to(self, entry, destination, &CancellationToken::new())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn copy_to(
        &self,
        target: &Store,
        entry: Entry,
        destination: String,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.copy_entries(target, &entry, &destination, cancel, |_, _| {})
            .await?;
        Ok(())
    }

    #[cfg(test)]
    async fn copy_entries(
        &self,
        target_store: &Store,
        entry: &Entry,
        destination: &str,
        cancel: &CancellationToken,
        mut report: impl FnMut(usize, usize) + Send,
    ) -> Result<Vec<Entry>> {
        let plan = self.plan_copy(target_store, entry, destination).await?;
        report(0, plan.len());
        for (index, (object, target)) in plan.iter().enumerate() {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            self.copy_object_to(target_store, object, target, cancel)
                .await
                .map_err(|e| {
                    if e.is::<TransferCancelled>() {
                        return e;
                    }
                    anyhow!(
                        "复制到 /{target} 失败，已复制 {index}/{} 个对象，源对象仍保留：{e:#}",
                        plan.len()
                    )
                })?;
            report(index + 1, plan.len());
        }
        Ok(plan.into_iter().map(|(object, _)| object).collect())
    }

    pub(super) async fn plan_copy(
        &self,
        target_store: &Store,
        entry: &Entry,
        destination: &str,
    ) -> Result<Vec<(Entry, String)>> {
        ensure!(
            !trash::reserved(&entry.key) && !trash::reserved(destination),
            "此路径由回收站保留。"
        );
        let same_bucket = self.bucket == target_store.bucket;
        ensure!(!same_bucket || entry.key != destination, "目标与源相同。");
        if entry.directory {
            ensure!(
                !same_bucket || !destination.starts_with(&entry.key),
                "目标不能位于源文件夹内部。"
            );
            ensure!(
                target_store.list(destination, false).await?.is_empty(),
                "目标文件夹已存在：/{destination}"
            );
        }
        let objects = self.objects(entry).await?;
        objects
            .into_iter()
            .map(|object| {
                let suffix = object
                    .key
                    .strip_prefix(&entry.key)
                    .context("S3 返回了前缀外的对象")?;
                let target = format!("{destination}{suffix}");
                ensure!(target.len() <= 1024, "对象键超过 1024 字节：{target}");
                ensure!(
                    object.size <= COPY_LIMIT,
                    "单个对象超过 5 GiB，无法执行复制：{}",
                    object.key
                );
                ensure!(
                    object.etag.is_some() || Self::is_directory_marker(&object),
                    "S3 未返回对象 ETag：{}",
                    object.key
                );
                Ok((object, target))
            })
            .collect()
    }

    pub(super) async fn copy_object(&self, object: &Entry, target: &str) -> Result<()> {
        self.copy_object_to(self, object, target, &CancellationToken::new())
            .await
            .map(|_| ())
    }

    pub(super) async fn copy_object_to(
        &self,
        target_store: &Store,
        object: &Entry,
        target: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        ensure!(
            object.size <= COPY_LIMIT,
            "单个对象超过 5 GiB：/{}",
            object.key
        );
        if Self::is_directory_marker(object) {
            self.verify_marker(object).await?;
            return target_store
                .put_conditional(target, ByteStream::from_static(b""), None)
                .await;
        }
        let etag = object.etag.as_deref().context("S3 未返回对象 ETag")?;
        let _permit = self
            .transfer_slots
            .acquire_worker(cancel, format!("S3 复制，/{} → /{target}", object.key))
            .await?;
        let source = format!(
            "{}/{}",
            self.bucket,
            object
                .key
                .split('/')
                .map(|part| utf8_percent_encode(part, NON_ALPHANUMERIC).to_string())
                .collect::<Vec<_>>()
                .join("/")
        );
        let output = target_store
            .client
            .copy_object()
            .bucket(&target_store.bucket)
            .key(target)
            .copy_source(source)
            .copy_source_if_match(etag)
            .if_none_match("*")
            .send()
            .await
            .map_err(|error| connection_error(&format!("无法复制到 /{target}"), error))?;
        Ok(output
            .copy_object_result()
            .and_then(|result| result.e_tag())
            .map(str::to_owned))
    }
}
