#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
use super::connection_error;
use super::{Entry, MIB, Store, TransferCancelled, TransferSlots};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, stream};
use md5::{Digest, Md5};
use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

impl Store {
    #[cfg(test)]
    pub(crate) async fn preview_text(&self, key: &str, limit: u64) -> Result<String> {
        self.preview_text_with_progress(key, None, limit, &AtomicU64::new(0))
            .await
    }

    #[cfg(test)]
    pub(crate) async fn preview_text_with_progress(
        &self,
        key: &str,
        etag: Option<&str>,
        limit: u64,
        received: &AtomicU64,
    ) -> Result<String> {
        let mut bytes = Vec::new();
        self.read_preview(key, etag, limit, &mut bytes, received)
            .await?;
        decode_preview_text(bytes)
    }

    #[cfg(test)]
    async fn read_preview(
        &self,
        key: &str,
        etag: Option<&str>,
        limit: u64,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
        received: &AtomicU64,
    ) -> Result<()> {
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .set_if_match(etag.map(str::to_owned))
            .send()
            .await
            .map_err(|error| connection_error(&format!("无法预览 /{key}"), error))?;
        ensure!(
            etag.zip(output.e_tag()).is_none_or(|(expected, actual)| {
                expected.trim_matches('"') == actual.trim_matches('"')
            }),
            "对象版本已变化，请刷新文件列表后重试"
        );
        let oversized = "文件超过当前预览上限，请使用下载。";
        ensure!(
            output
                .content_length()
                .is_none_or(|length| length <= limit as i64),
            "{oversized}"
        );
        let expected = output.content_length();
        let mut body = output.body.into_async_read();
        let mut buffer = vec![0; 64 * 1024];
        let mut total = 0;
        loop {
            let length = body.read(&mut buffer).await.context("无法读取预览内容")?;
            if length == 0 {
                break;
            }
            total += length as u64;
            ensure!(total <= limit, "{oversized}");
            writer.write_all(&buffer[..length]).await?;
            received.store(total, Ordering::Relaxed);
        }
        ensure!(
            expected.is_none_or(|length| length == total as i64),
            "预览下载不完整"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn download(&self, key: String, path: PathBuf) -> Result<()> {
        self.download_conditional(&key, &path, None, true).await
    }

    pub(crate) async fn download_with_progress(
        &self,
        entry: &Entry,
        path: &Path,
        received: &AtomicU64,
        slots: &TransferSlots,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.download_file(entry, path, received, slots, cancel, None)
            .await
    }

    pub(crate) async fn matches_contents(
        &self,
        entry: &Entry,
        digest: &[u8; 16],
        slots: &TransferSlots,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let key = entry.key();
        let size = u64::try_from(entry.size())?;
        let etag = entry.etag().context("S3 未返回对象 ETag")?;
        let mut permit = slots
            .acquire_download_worker(cancel, format!("核对内容，/{key}"))
            .await?;
        let expected_md5 = super::hex_digest(digest);
        let compare = async {
            if etag.trim_matches('"').eq_ignore_ascii_case(&expected_md5)
                && self
                    .head(key)
                    .await
                    .is_ok_and(|tag| tag.matches_md5(etag, entry.size(), &expected_md5))
            {
                return Ok(true);
            }
            let output = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .if_match(etag)
                .send()
                .await
                .context("无法读取对象以核对内容")?;
            ensure!(
                output
                    .e_tag()
                    .is_none_or(|actual| actual.trim_matches('"') == etag.trim_matches('"')),
                "核对时对象版本已变化"
            );
            let worker_bytes = permit.track_bytes(Some(size));
            let mut input = output.body.into_async_read();
            let mut hash = Md5::new();
            let mut buffer = vec![0; 256 * 1024];
            let mut received = 0;
            loop {
                let count = input.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                received += count as u64;
                ensure!(received <= size, "核对时对象长度已变化");
                hash.update(&buffer[..count]);
                worker_bytes.record(count as u64, count as u64);
            }
            ensure!(received == size, "核对时对象长度已变化");
            Ok(hash.finalize().as_slice() == digest)
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(TransferCancelled.into()),
            result = compare => result,
        }
    }

    pub(crate) async fn download_preview_with_workers(
        &self,
        entry: &Entry,
        path: &Path,
        limit: u64,
        received: &AtomicU64,
        slots: &TransferSlots,
    ) -> Result<()> {
        ensure!(
            entry.size().max(0) as u64 <= limit,
            "文件超过当前预览上限，请使用下载。"
        );
        self.download_file(
            entry,
            path,
            received,
            slots,
            &CancellationToken::new(),
            Some(limit),
        )
        .await
    }

    async fn download_file(
        &self,
        entry: &Entry,
        path: &Path,
        received: &AtomicU64,
        slots: &TransferSlots,
        cancel: &CancellationToken,
        preview_limit: Option<u64>,
    ) -> Result<()> {
        let key = entry.key();
        let size = entry.size().max(0) as u64;
        let temp = tempfile::NamedTempFile::new_in(path.parent().context("保存路径没有上级目录")?)?;
        let segment = 16 * MIB;
        let parallel = size > 64 * MIB && entry.etag().is_some();
        let fetch = |start: u64, length: u64, ranged: bool| {
            let temp = &temp;
            async move {
                let end = start + length.saturating_sub(1);
                let label = if ranged {
                    format!("下载，/{key}，字节 {start}–{end}")
                } else {
                    format!("下载，/{key}")
                };
                let mut permit = slots.acquire_download_worker(cancel, label).await?;
                let output = self
                    .client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .set_if_match(entry.etag().map(str::to_owned))
                    .set_range(ranged.then(|| format!("bytes={start}-{end}")))
                    .send()
                    .await
                    .context("无法下载对象")?;
                let expected = output.content_length();
                if let Some(limit) = preview_limit {
                    ensure!(
                        entry
                            .etag()
                            .zip(output.e_tag())
                            .is_none_or(|(expected, actual)| {
                                expected.trim_matches('"') == actual.trim_matches('"')
                            }),
                        "对象版本已变化，请刷新文件列表后重试"
                    );
                    ensure!(
                        expected.is_none_or(|length| length as u64 <= limit),
                        "文件超过当前预览上限，请使用下载。"
                    );
                }
                if ranged {
                    let expected = format!("bytes {start}-{end}/{size}");
                    if output.content_range().is_none() {
                        return Ok::<_, anyhow::Error>(false);
                    }
                    ensure!(
                        output.content_range() == Some(expected.as_str()),
                        "服务端返回了错误的下载范围"
                    );
                }
                let worker_bytes = permit.track_bytes(if ranged {
                    Some(length)
                } else {
                    expected.and_then(|length| u64::try_from(length).ok())
                });
                let mut input = output.body.into_async_read();
                let mut file = tokio::fs::File::from_std(temp.reopen()?);
                file.seek(std::io::SeekFrom::Start(start)).await?;
                let mut buffer = vec![0; 256 * 1024];
                let mut written = 0;
                loop {
                    let count = input.read(&mut buffer).await?;
                    if count == 0 {
                        break;
                    }
                    written += count as u64;
                    ensure!(
                        preview_limit.is_none_or(|limit| written <= limit),
                        "文件超过当前预览上限，请使用下载。"
                    );
                    ensure!(!ranged || written <= length, "分段下载返回的数据过长");
                    file.write_all(&buffer[..count]).await?;
                    received.fetch_add(count as u64, Ordering::Relaxed);
                    worker_bytes.record(count as u64, count as u64);
                }
                ensure!(!ranged || written == length, "分段下载的数据不完整");
                ensure!(
                    expected.is_none_or(|length| written == length as u64),
                    "下载不完整"
                );
                file.flush().await?;
                Ok(true)
            }
        };
        let download = async {
            if parallel && fetch(0, 1, true).await? {
                use futures_util::TryStreamExt;
                stream::iter(0..(size - 1).div_ceil(segment))
                    .map(|index| async move {
                        let start = 1 + index * segment;
                        ensure!(
                            fetch(start, (size - start).min(segment), true).await?,
                            "服务端在分段下载过程中停止支持范围请求"
                        );
                        Ok(())
                    })
                    .buffer_unordered(crate::services::settings::MAX_TRANSFER_PARALLELISM)
                    .try_collect::<Vec<()>>()
                    .await?;
            } else {
                fetch(0, size, false).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!(TransferCancelled),
            result = download => result?,
        }
        temp.as_file().sync_all()?;
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        temp.persist(path)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn download_conditional(
        &self,
        key: &str,
        path: &Path,
        etag: Option<&str>,
        replace: bool,
    ) -> Result<()> {
        let parent = path.parent().context("保存路径没有上级目录")?;
        let temp = tempfile::NamedTempFile::new_in(parent)?;
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .set_if_match(etag.map(str::to_owned))
            .send()
            .await
            .map_err(|error| connection_error(&format!("无法下载 /{key}"), error))?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(temp.path())
            .await?;
        tokio::io::copy(&mut output.body.into_async_read(), &mut file).await?;
        file.sync_all().await?;
        drop(file);
        if replace {
            temp.persist(path)?;
        } else {
            temp.persist_noclobber(path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl TextEncoding {
    pub(crate) fn encode(self, text: &str) -> Vec<u8> {
        match self {
            Self::Utf8 => text.as_bytes().to_vec(),
            Self::Utf8Bom => [b"\xef\xbb\xbf", text.as_bytes()].concat(),
            Self::Utf16Le => [0xff, 0xfe]
                .into_iter()
                .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
                .collect(),
            Self::Utf16Be => [0xfe, 0xff]
                .into_iter()
                .chain(text.encode_utf16().flat_map(u16::to_be_bytes))
                .collect(),
        }
    }
}

#[cfg(test)]
pub(crate) fn decode_preview_text(bytes: Vec<u8>) -> Result<String> {
    decode_editable_text(bytes).map(|(text, _)| text)
}

pub(crate) fn decode_editable_text(mut bytes: Vec<u8>) -> Result<(String, TextEncoding)> {
    let encoding_error = "无法解码此文本，请使用下载。";
    let (text, encoding) = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little_endian = bytes[0] == 0xff;
        let (pairs, remainder) = bytes[2..].as_chunks::<2>();
        ensure!(remainder.is_empty(), "{encoding_error}");
        let text = char::decode_utf16(pairs.iter().copied().map(|pair| {
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        }))
        .collect::<std::result::Result<String, _>>()
        .context(encoding_error)?;
        (
            text,
            if little_endian {
                TextEncoding::Utf16Le
            } else {
                TextEncoding::Utf16Be
            },
        )
    } else {
        let encoding = if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            bytes.drain(..3);
            TextEncoding::Utf8Bom
        } else {
            TextEncoding::Utf8
        };
        (String::from_utf8(bytes).context(encoding_error)?, encoding)
    };
    ensure!(!text.contains('\0'), "文件包含二进制内容，请使用下载。");
    Ok((text, encoding))
}
