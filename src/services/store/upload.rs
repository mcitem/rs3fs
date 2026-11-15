#[cfg(test)]
use super::upload_plan;
use super::{COPY_LIMIT, MIB, Store, connection_error, trash};
use anyhow::{Context, Result, bail, ensure};
use aws_sdk_s3::{
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use futures_util::{StreamExt, stream};
use http_body_util::BodyExt;
#[cfg(test)]
use std::path::PathBuf;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(super) fn upload_part_size(size: u64) -> u64 {
    (16 * MIB).max(size.div_ceil(10_000))
}

#[derive(Debug)]
pub(crate) struct TransferCancelled;

impl std::fmt::Display for TransferCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("传输已取消")
    }
}

impl std::error::Error for TransferCancelled {}

#[derive(Clone)]
pub(crate) struct TransferSlots(watch::Sender<TransferSlotState>);

pub(crate) struct TransferSlotState {
    limit: usize,
    workers: Vec<Option<String>>,
}

impl TransferSlots {
    pub(crate) fn new(limit: usize) -> Self {
        Self(
            watch::channel(TransferSlotState {
                limit,
                workers: Vec::new(),
            })
            .0,
        )
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        self.0.send_modify(|state| state.limit = limit);
    }

    pub(crate) fn workers(&self) -> (usize, Vec<(usize, String)>) {
        let state = self.0.borrow();
        let workers = state
            .workers
            .iter()
            .enumerate()
            .filter_map(|(index, worker)| Some((index + 1, worker.as_ref()?.clone())))
            .collect();
        (state.limit, workers)
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<TransferSlotState> {
        self.0.subscribe()
    }

    pub(crate) async fn acquire_worker(
        &self,
        cancel: &CancellationToken,
        label: String,
    ) -> Result<TransferPermit<'_>> {
        let mut state = self.0.subscribe();
        let mut label = Some(label);
        loop {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            let mut permit = None;
            if self.0.send_if_modified(|state| {
                if state.workers.iter().flatten().count() >= state.limit {
                    return false;
                }
                let index = state
                    .workers
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or(state.workers.len());
                if index == state.workers.len() {
                    state.workers.push(label.take());
                } else {
                    state.workers[index] = label.take();
                }
                permit = Some(TransferPermit { slots: self, index });
                true
            }) {
                return Ok(permit.unwrap());
            }
            tokio::select! {
                _ = cancel.cancelled() => bail!(TransferCancelled),
                _ = state.changed() => {}
            }
        }
    }
}

pub(crate) struct TransferPermit<'a> {
    slots: &'a TransferSlots,
    index: usize,
}

impl Drop for TransferPermit<'_> {
    fn drop(&mut self) {
        self.slots
            .0
            .send_modify(|state| state.workers[self.index] = None);
    }
}

pub(crate) struct UploadTransfer<'a> {
    pub(crate) sent: Arc<AtomicU64>,
    pub(crate) slots: &'a TransferSlots,
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) on_start: &'a (dyn Fn() + Send + Sync),
}

#[derive(Default, serde::Deserialize, serde::Serialize)]
struct MultipartCheckpoint {
    id: String,
    parts: std::collections::BTreeMap<u64, String>,
}

#[derive(Debug)]
struct ExpiredMultipart;

impl std::fmt::Display for ExpiredMultipart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("服务端的分片上传记录已失效，继续后将重新上传此文件")
    }
}

impl std::error::Error for ExpiredMultipart {}

impl Store {
    pub(crate) async fn save_text(
        &self,
        key: &str,
        etag: &str,
        bytes: Vec<u8>,
    ) -> Result<Option<String>> {
        ensure!(!trash::reserved(key), "此路径由回收站保留。");
        let original = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .if_match(etag)
            .send()
            .await
            .map_err(|error| connection_error(&format!("无法读取 /{key} 的对象信息"), error))?;
        let output = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_match(etag)
            .set_content_type(original.content_type)
            .set_content_language(original.content_language)
            .set_content_disposition(original.content_disposition)
            .set_cache_control(original.cache_control)
            .set_content_encoding(original.content_encoding)
            .set_expires(original.expires_string.as_deref().and_then(|value| {
                aws_smithy_types::DateTime::from_str(
                    value,
                    aws_smithy_types::date_time::Format::HttpDate,
                )
                .ok()
            }))
            .set_metadata(original.metadata)
            .set_storage_class(original.storage_class)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|error| connection_error(&format!("无法保存 /{key}"), error))?;
        Ok(output.e_tag)
    }

    pub(crate) async fn mkdir(&self, key: String) -> Result<()> {
        ensure!(
            self.list(&key, true).await?.is_empty(),
            "文件夹已存在：/{key}"
        );
        self.put(&key, ByteStream::from_static(b"")).await
    }

    pub(crate) async fn create_file(&self, key: String) -> Result<()> {
        ensure!(!trash::reserved(&key), "此路径由回收站保留。");
        self.put(&key, ByteStream::from_static(b"")).await
    }

    pub(super) async fn put(&self, key: &str, body: ByteStream) -> Result<()> {
        self.put_conditional(key, body, None).await.map(|_| ())
    }

    pub(super) async fn put_conditional(
        &self,
        key: &str,
        body: ByteStream,
        etag: Option<&str>,
    ) -> Result<Option<String>> {
        ensure!(key.len() <= 1024, "对象键超过 1024 字节：{key}");
        let output = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .set_if_none_match(etag.is_none().then(|| "*".to_owned()))
            .set_if_match(etag.map(str::to_owned))
            .body(body)
            .send()
            .await
            .map_err(|error| connection_error(&format!("无法写入 /{key}"), error))?;
        Ok(output.e_tag().map(str::to_owned))
    }

    #[cfg(test)]
    pub(crate) async fn upload_file(
        &self,
        key: &str,
        path: &Path,
        size: u64,
        sent: Arc<AtomicU64>,
        etag: Option<&str>,
    ) -> Result<Option<String>> {
        self.transfer_file(
            key,
            path,
            size,
            etag,
            UploadTransfer {
                sent,
                slots: &TransferSlots::new(1),
                cancel: &CancellationToken::new(),
                on_start: &|| {},
            },
        )
        .await
    }

    pub(crate) async fn transfer_file(
        &self,
        key: &str,
        path: &Path,
        size: u64,
        etag: Option<&str>,
        transfer: UploadTransfer<'_>,
    ) -> Result<Option<String>> {
        self.transfer_file_checkpointed(key, path, size, etag, transfer, None)
            .await
    }

    pub(super) async fn transfer_file_checkpointed(
        &self,
        key: &str,
        path: &Path,
        size: u64,
        etag: Option<&str>,
        transfer: UploadTransfer<'_>,
        checkpoint: Option<(&crate::services::transfer_journal::TransferJournal, usize)>,
    ) -> Result<Option<String>> {
        let UploadTransfer {
            sent,
            slots,
            cancel,
            on_start,
        } = transfer;
        if size <= 64 * MIB {
            let _permit = slots
                .acquire_worker(cancel, format!("上传，/{key}"))
                .await?;
            on_start();
            let body = tracked_body(ByteStream::from_path(path).await?, sent);
            return tokio::select! {
                biased;
                result = self.put_conditional(key, body, etag) => result,
                _ = cancel.cancelled() => bail!(TransferCancelled),
            };
        }
        ensure!(key.len() <= 1024, "对象键超过 1024 字节：{key}");
        let part_size = upload_part_size(size);
        ensure!(part_size <= COPY_LIMIT as u64, "文件超过分片上传上限。");
        let count = size.div_ceil(part_size);
        let checkpoint_name = checkpoint.map(|(_, index)| format!("multipart-{index}.json"));
        let disk = checkpoint
            .zip(checkpoint_name.as_deref())
            .map(|((journal, _), name)| (journal, name));
        let mut saved = if let Some((journal, name)) = disk {
            journal
                .load::<Option<MultipartCheckpoint>>(name)?
                .flatten()
                .unwrap_or_default()
        } else {
            MultipartCheckpoint::default()
        };
        ensure!(
            saved.parts.keys().all(|index| *index < count),
            "上传检查点中的分片编号不正确"
        );
        if saved.id.is_empty() {
            let _permit = slots
                .acquire_worker(cancel, format!("/{key}，创建分片上传"))
                .await?;
            on_start();
            let upload = self
                .client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;
            saved.id = upload
                .upload_id()
                .context("S3 未返回分片上传 ID")?
                .to_owned();
            if let Some((journal, name)) = disk {
                journal.save(name, &Some(&saved))?;
            }
        } else {
            on_start();
        }
        let id = saved.id.clone();
        let saved_bytes = saved
            .parts
            .keys()
            .map(|index| (size - index * part_size).min(part_size))
            .sum();
        sent.store(saved_bytes, Ordering::Relaxed);
        let result = async {
            let stop = cancel.child_token();
            let remaining: Vec<_> = (0..count)
                .filter(|index| !saved.parts.contains_key(index))
                .collect();
            let id = id.as_str();
            let mut uploads = stream::iter(remaining)
                .map(|index| {
                    let sent = sent.clone();
                    let stop = &stop;
                    async move {
                        let number = index as i32 + 1;
                        let _permit = slots
                            .acquire_worker(stop, format!("上传，/{key}，分片 {number}/{count}"))
                            .await?;
                        let offset = index * part_size;
                        let length = (size - offset).min(part_size);
                        let body = ByteStream::read_from()
                            .path(path)
                            .offset(offset)
                            .length(aws_smithy_types::byte_stream::Length::Exact(length))
                            .build()
                            .await?;
                        let part = self
                            .client
                            .upload_part()
                            .bucket(&self.bucket)
                            .key(key)
                            .upload_id(id)
                            .part_number(number)
                            .body(tracked_body(body, sent))
                            .send()
                            .await
                            .map_err(|error| {
                                if disk.is_some()
                                    && error
                                        .raw_response()
                                        .is_some_and(|response| response.status().as_u16() == 404)
                                {
                                    anyhow::Error::from(ExpiredMultipart)
                                } else {
                                    anyhow::Error::from(error)
                                }
                            })?;
                        Ok::<_, anyhow::Error>((
                            index,
                            part.e_tag().context("S3 未返回分片 ETag")?.to_owned(),
                        ))
                    }
                })
                .buffer_unordered(crate::services::settings::MAX_TRANSFER_PARALLELISM);
            let mut failure = None;
            while let Some(part) = uploads.next().await {
                match part {
                    Ok((index, etag)) => {
                        saved.parts.insert(index, etag);
                        if let Some((journal, name)) = disk
                            && let Err(error) = journal.save(name, &Some(&saved))
                        {
                            failure = Some(error);
                            stop.cancel();
                        }
                    }
                    Err(error) => {
                        if failure.as_ref().is_none_or(|previous: &anyhow::Error| {
                            previous.is::<TransferCancelled>() && !error.is::<TransferCancelled>()
                        }) {
                            failure = Some(error);
                        }
                        stop.cancel();
                    }
                }
            }
            if let Some(error) = failure {
                return Err(error);
            }
            let _permit = slots
                .acquire_worker(cancel, format!("/{key}，合并分片"))
                .await?;
            let parts = saved
                .parts
                .iter()
                .map(|(index, etag)| {
                    CompletedPart::builder()
                        .part_number(*index as i32 + 1)
                        .e_tag(etag)
                        .build()
                })
                .collect();
            let output = self
                .client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(id)
                .set_if_none_match(etag.is_none().then(|| "*".to_owned()))
                .set_if_match(etag.map(str::to_owned))
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .map_err(|error| {
                    if disk.is_some()
                        && error
                            .raw_response()
                            .is_some_and(|response| response.status().as_u16() == 404)
                    {
                        anyhow::Error::from(ExpiredMultipart)
                    } else {
                        anyhow::Error::from(error)
                    }
                })?;
            Ok::<_, anyhow::Error>(output.e_tag().map(str::to_owned))
        }
        .await;
        match result {
            Ok(etag) => Ok(etag),
            Err(error) if checkpoint.is_some() && !cancel.is_cancelled() => {
                if error.is::<ExpiredMultipart>()
                    && let Some((journal, name)) = disk
                {
                    journal.save(name, &Option::<MultipartCheckpoint>::None)?;
                }
                Err(error)
            }
            Err(error) => {
                let cleanup = CancellationToken::new();
                let _permit = slots
                    .acquire_worker(&cleanup, format!("/{key}，清理分片"))
                    .await?;
                let abort = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&id)
                    .send()
                    .await;
                if let Err(abort) = abort {
                    let cleanup = connection_error("清理分片上传失败", abort);
                    bail!("分片上传失败：{error:#}。{cleanup}");
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn transfer_marker(
        &self,
        key: &str,
        slots: &TransferSlots,
        cancel: &CancellationToken,
        on_start: impl FnOnce(),
    ) -> Result<()> {
        let _permit = slots.acquire_worker(cancel, format!("/{key}")).await?;
        on_start();
        tokio::select! {
            biased;
            result = self.put(key, ByteStream::from_static(b"")) => result,
            _ = cancel.cancelled() => bail!(TransferCancelled),
        }
    }

    #[cfg(test)]
    pub(crate) async fn upload(
        &self,
        prefix: String,
        paths: Vec<PathBuf>,
        mut progress: impl FnMut(usize, usize, &str, Option<(u64, Arc<AtomicU64>)>),
    ) -> Result<usize> {
        let plan = tokio::task::spawn_blocking(move || upload_plan(&prefix, paths)).await??;
        ensure!(
            plan.iter().all(|(key, _)| !trash::reserved(key)),
            "上传路径与回收站保留路径冲突。"
        );
        for (index, (key, path)) in plan.iter().enumerate() {
            let result = match path {
                Some(path) => {
                    let size = tokio::fs::metadata(path).await?.len();
                    let sent = Arc::new(AtomicU64::new(0));
                    progress(index, plan.len(), key, Some((size, sent.clone())));
                    self.upload_file(key, path, size, sent, None)
                        .await
                        .map(|_| ())
                }
                None => {
                    progress(index, plan.len(), key, None);
                    self.put(key, ByteStream::from_static(b"")).await
                }
            };
            result
                .with_context(|| format!("上传在 /{key} 中断，已写入 {index}/{} 项", plan.len()))?;
        }
        Ok(plan.len())
    }
}

pub(super) fn tracked_body(body: ByteStream, sent: Arc<AtomicU64>) -> ByteStream {
    use aws_smithy_types::body::SdkBody;
    let highest = Arc::new(AtomicU64::new(0));
    body.map(move |body| {
        let sent = sent.clone();
        let highest = highest.clone();
        let attempt = AtomicU64::new(0);
        SdkBody::from_body_1_x(body.inspect_frame(move |frame| {
            if let Some(data) = frame.data_ref() {
                let bytes =
                    attempt.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;
                let previous = highest.fetch_max(bytes, Ordering::Relaxed);
                sent.fetch_add(bytes.saturating_sub(previous), Ordering::Relaxed);
            }
        }))
    })
}
