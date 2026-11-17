use super::multipart_copy::{CopyMetadata, apply_copy_metadata};
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
    time::Instant,
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
    workers: Vec<Option<Worker>>,
    upload: DirectionTraffic,
    download: DirectionTraffic,
    sampled_at: Instant,
}

#[derive(Default)]
struct DirectionTraffic {
    retired: u64,
    sampled: u64,
    speed: Option<f64>,
}

impl DirectionTraffic {
    fn sample(&mut self, active: bool, active_bytes: u64, seconds: f64) -> bool {
        let total = self.retired + active_bytes;
        let bytes = total - self.sampled;
        let speed = (active || bytes > 0).then_some(bytes as f64 / seconds);
        let changed = self.speed != speed;
        self.speed = speed;
        self.sampled = total;
        changed
    }
}

struct Worker {
    label: String,
    kind: WorkerKind,
    progress: Option<WorkerProgress>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerKind {
    Upload,
    Download,
    ServerCopy,
}

struct WorkerProgress {
    bytes: Arc<WorkerBytes>,
    total: Option<u64>,
    sampled: u64,
    sample_time: Instant,
    speed: f64,
}

#[derive(Default)]
pub(super) struct WorkerBytes {
    pub(super) traffic: AtomicU64,
    transferred: AtomicU64,
}

impl WorkerBytes {
    pub(super) fn record(&self, traffic: u64, transferred: u64) {
        self.transferred.fetch_add(transferred, Ordering::Relaxed);
        self.traffic.fetch_add(traffic, Ordering::Relaxed);
    }
}

pub(crate) struct WorkerSnapshot {
    pub(crate) id: usize,
    pub(crate) label: String,
    pub(crate) speed: Option<f64>,
    pub(crate) progress: Option<(u64, Option<u64>)>,
}

impl TransferSlotState {
    pub(crate) fn has_measured_workers(&self) -> bool {
        [&self.upload, &self.download]
            .into_iter()
            .any(|traffic| traffic.retired != traffic.sampled || traffic.speed.is_some())
            || self
                .workers
                .iter()
                .flatten()
                .any(|worker| worker.progress.is_some())
    }
}

impl TransferSlots {
    pub(crate) fn limit(&self) -> usize {
        self.0.borrow().limit
    }

    pub(crate) fn new(limit: usize) -> Self {
        Self(
            watch::channel(TransferSlotState {
                limit,
                workers: Vec::new(),
                upload: DirectionTraffic::default(),
                download: DirectionTraffic::default(),
                sampled_at: Instant::now(),
            })
            .0,
        )
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        self.0.send_modify(|state| state.limit = limit);
    }

    pub(crate) fn workers(&self) -> (usize, Vec<WorkerSnapshot>) {
        let state = self.0.borrow();
        let workers = state
            .workers
            .iter()
            .enumerate()
            .filter_map(|(index, worker)| {
                let worker = worker.as_ref()?;
                Some(WorkerSnapshot {
                    id: index + 1,
                    label: worker.label.clone(),
                    speed: worker.progress.as_ref().map(|progress| progress.speed),
                    progress: worker.progress.as_ref().map(|progress| {
                        (
                            progress.bytes.transferred.load(Ordering::Relaxed),
                            progress.total,
                        )
                    }),
                })
            })
            .collect();
        (state.limit, workers)
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<TransferSlotState> {
        self.0.subscribe()
    }

    pub(crate) fn direction_speeds(&self) -> (Option<i64>, Option<i64>) {
        let state = self.0.borrow();
        let speed = |kind, traffic: &DirectionTraffic| {
            traffic.speed.map(|speed| speed as i64).or_else(|| {
                state
                    .workers
                    .iter()
                    .flatten()
                    .any(|worker| worker.kind == kind && worker.progress.is_some())
                    .then_some(0)
            })
        };
        (
            speed(WorkerKind::Upload, &state.upload),
            speed(WorkerKind::Download, &state.download),
        )
    }

    pub(crate) fn sample_workers(&self) {
        let now = Instant::now();
        self.0.send_if_modified(|state| {
            let mut changed = false;
            let seconds = now
                .saturating_duration_since(state.sampled_at)
                .as_secs_f64();
            let mut upload = (false, 0);
            let mut download = (false, 0);
            for worker in state.workers.iter_mut().flatten() {
                if let Some(progress) = &mut worker.progress {
                    let bytes = progress.bytes.traffic.load(Ordering::Relaxed);
                    let direction = match worker.kind {
                        WorkerKind::Upload => Some(&mut upload),
                        WorkerKind::Download => Some(&mut download),
                        WorkerKind::ServerCopy => None,
                    };
                    if let Some(direction) = direction {
                        direction.0 = true;
                        direction.1 += bytes;
                    }
                    let elapsed = now
                        .saturating_duration_since(progress.sample_time)
                        .as_secs_f64();
                    if elapsed > 0.0 {
                        let speed = (bytes - progress.sampled) as f64 / elapsed;
                        changed |= progress.speed != speed || bytes != progress.sampled;
                        progress.speed = speed;
                        progress.sampled = bytes;
                        progress.sample_time = now;
                    }
                }
            }
            if seconds > 0.0 {
                changed |= state.upload.sample(upload.0, upload.1, seconds);
                changed |= state.download.sample(download.0, download.1, seconds);
                state.sampled_at = now;
            }
            changed
        });
    }

    pub(crate) async fn acquire_worker(
        &self,
        cancel: &CancellationToken,
        label: String,
    ) -> Result<TransferPermit<'_>> {
        self.acquire(cancel, label, WorkerKind::Upload).await
    }

    pub(crate) async fn acquire_download_worker(
        &self,
        cancel: &CancellationToken,
        label: String,
    ) -> Result<TransferPermit<'_>> {
        self.acquire(cancel, label, WorkerKind::Download).await
    }

    pub(super) async fn acquire_copy_worker(
        &self,
        cancel: &CancellationToken,
        label: String,
    ) -> Result<TransferPermit<'_>> {
        self.acquire(cancel, label, WorkerKind::ServerCopy).await
    }

    async fn acquire(
        &self,
        cancel: &CancellationToken,
        label: String,
        kind: WorkerKind,
    ) -> Result<TransferPermit<'_>> {
        let mut state = self.0.subscribe();
        let mut worker = Some(Worker {
            label,
            kind,
            progress: None,
        });
        loop {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            let mut permit = None;
            if self.0.send_if_modified(|state| {
                if state
                    .workers
                    .iter()
                    .flatten()
                    .filter(|worker| worker.kind == kind)
                    .count()
                    >= state.limit
                {
                    return false;
                }
                let index = state
                    .workers
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or(state.workers.len());
                if index == state.workers.len() {
                    state.workers.push(worker.take());
                } else {
                    state.workers[index] = worker.take();
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

impl TransferPermit<'_> {
    pub(super) fn track_bytes(&mut self, total: Option<u64>) -> Arc<WorkerBytes> {
        let bytes = Arc::new(WorkerBytes::default());
        self.slots.0.send_modify(|state| {
            if !state.has_measured_workers() {
                state.sampled_at = Instant::now();
            }
            state.workers[self.index].as_mut().unwrap().progress = Some(WorkerProgress {
                bytes: bytes.clone(),
                total,
                sampled: 0,
                sample_time: Instant::now(),
                speed: 0.0,
            });
        });
        bytes
    }
}

impl Drop for TransferPermit<'_> {
    fn drop(&mut self) {
        self.slots.0.send_modify(|state| {
            let Some(worker) = state.workers[self.index].take() else {
                return;
            };
            if let Some(progress) = worker.progress {
                let traffic = match worker.kind {
                    WorkerKind::Upload => &mut state.upload,
                    WorkerKind::Download => &mut state.download,
                    WorkerKind::ServerCopy => return,
                };
                traffic.retired += progress.bytes.traffic.load(Ordering::Relaxed);
            }
        });
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
        let metadata = CopyMetadata {
            tags: self.object_tags(key, &original).await?,
            original,
        };
        let request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_match(etag);
        let output = apply_copy_metadata!(request, &metadata)
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
        self.put_with_metadata(key, body, etag, None).await
    }

    async fn put_with_metadata(
        &self,
        key: &str,
        body: ByteStream,
        etag: Option<&str>,
        metadata: Option<&CopyMetadata>,
    ) -> Result<Option<String>> {
        ensure!(key.len() <= 1024, "对象键超过 1024 字节：{key}");
        let request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .set_if_none_match(etag.is_none().then(|| "*".to_owned()))
            .set_if_match(etag.map(str::to_owned))
            .body(body);
        let request = match metadata {
            Some(metadata) => apply_copy_metadata!(request, metadata),
            None => request,
        };
        let output = request
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
        self.transfer_file_with_metadata(key, path, size, etag, transfer, checkpoint, None)
            .await
    }

    pub(super) async fn transfer_file_with_metadata(
        &self,
        key: &str,
        path: &Path,
        size: u64,
        etag: Option<&str>,
        transfer: UploadTransfer<'_>,
        checkpoint: Option<(&crate::services::transfer_journal::TransferJournal, usize)>,
        metadata: Option<&CopyMetadata>,
    ) -> Result<Option<String>> {
        let UploadTransfer {
            sent,
            slots,
            cancel,
            on_start,
        } = transfer;
        if size <= 64 * MIB {
            let mut permit = slots
                .acquire_worker(cancel, format!("上传，/{key}"))
                .await?;
            on_start();
            let body = tracked_body(
                ByteStream::from_path(path).await?,
                sent,
                permit.track_bytes(Some(size)),
            );
            return tokio::select! {
                biased;
                result = self.put_with_metadata(key, body, etag, metadata) => result,
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
            let request = self
                .client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(key);
            let request = match metadata {
                Some(metadata) => apply_copy_metadata!(request, metadata),
                None => request,
            };
            let upload = request.send().await?;
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
                        let mut permit = slots
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
                            .body(tracked_body(body, sent, permit.track_bytes(Some(length))))
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
            Err(error)
                if checkpoint.is_some_and(|(journal, _)| {
                    !cancel.is_cancelled() || journal.is_pause_requested()
                }) =>
            {
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

pub(super) fn tracked_body(
    body: ByteStream,
    sent: Arc<AtomicU64>,
    worker_bytes: Arc<WorkerBytes>,
) -> ByteStream {
    use aws_smithy_types::body::SdkBody;
    let highest = Arc::new(AtomicU64::new(0));
    body.map(move |body| {
        let sent = sent.clone();
        let worker_bytes = worker_bytes.clone();
        let highest = highest.clone();
        let attempt = AtomicU64::new(0);
        SdkBody::from_body_1_x(body.inspect_frame(move |frame| {
            if let Some(data) = frame.data_ref() {
                let bytes =
                    attempt.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;
                let previous = highest.fetch_max(bytes, Ordering::Relaxed);
                let transferred = bytes.saturating_sub(previous);
                sent.fetch_add(transferred, Ordering::Relaxed);
                worker_bytes.record(data.len() as u64, transferred);
            }
        }))
    })
}
