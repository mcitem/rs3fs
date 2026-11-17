use super::{Store, connection_error, upload::upload_part_size};
use anyhow::{Context, Result, ensure};
use std::path::Path;
use tokio::io::AsyncReadExt;

impl Store {
    pub(crate) async fn compare_etag_progress(
        &self,
        key: &str,
        tag: &ObjectTag,
        path: &Path,
        progress: impl Fn(u64),
    ) -> Result<String> {
        let Some((_, count)) = tag.multipart() else {
            return tag.compare_progress(path, progress).await;
        };
        let size = tokio::fs::metadata(path).await?.len();
        if tag
            .size
            .is_some_and(|remote| u64::try_from(remote) != Ok(size))
        {
            return Ok("文件大小不同".into());
        }
        if !tag.md5_candidate || tag.size.is_none() {
            return Ok("远端加密方式或对象元数据不支持分片 MD5 比对。".into());
        }
        let mut lengths = Vec::with_capacity(count as usize);
        let mut offset = 0;
        for number in 1..=count {
            let response = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .part_number(number)
                .set_version_id(tag.version.clone())
                .send()
                .await;
            let part = match response {
                Ok(part) => part,
                Err(error)
                    if error
                        .raw_response()
                        .is_some_and(|res| res.status().as_u16() == 412) =>
                {
                    return Ok("远端对象已变化，请重新比对。".into());
                }
                Err(error)
                    if error.raw_response().is_some_and(|res| {
                        matches!(res.status().as_u16(), 400 | 405 | 416 | 501)
                    }) =>
                {
                    break;
                }
                Err(error) => return Err(connection_error("无法读取分片元数据", error)),
            };
            let length = part
                .content_length()
                .and_then(|length| u64::try_from(length).ok())
                .unwrap_or(0);
            let end = offset + length;
            let valid = length > 0
                && end <= size
                && part.parts_count().is_none_or(|parts| parts == count)
                && (part.parts_count().is_some() || part.content_range().is_some())
                && part
                    .content_range()
                    .is_none_or(|range| range == format!("bytes {offset}-{}/{size}", end - 1));
            if !valid {
                break;
            }
            lengths.push(length);
            offset = end;
            progress(25 * number as u64 / count as u64);
        }
        let verified = lengths.len() == count as usize && offset == size;
        let current = self.head(key).await?;
        if current.value() != tag.value()
            || current.size != tag.size
            || current.version != tag.version
        {
            return Ok("远端对象已变化，请重新比对。".into());
        }
        if !verified {
            let part_size = upload_part_size(size);
            if size == 0 || size.div_ceil(part_size) != count as u64 {
                return Ok("端点未返回可验证的分片边界，无法比对分片 ETag。".into());
            }
            lengths = (0..count as u64)
                .map(|index| (size - index * part_size).min(part_size))
                .collect();
        }
        let local =
            multipart_etag_progress(tokio::fs::File::open(path).await?, &lengths, |bytes| {
                progress(25 + 75 * bytes / size.max(1))
            })
            .await?;
        let matches = local.eq_ignore_ascii_case(tag.value().trim_matches('"'));
        if !verified {
            return Ok(if matches {
                format!(
                    "本地分片 ETag 与远端一致（按本程序的上传分片规则试算）\n本地分片 ETag：{local}"
                )
            } else {
                "端点未提供可验证的分片边界。按本程序的分片规则试算未匹配，无法判断内容是否相同。"
                    .into()
            });
        }
        let status = if matches { "一致" } else { "不一致" };
        Ok(format!(
            "本地分片 ETag 与远端{status}\n本地分片 ETag：{local}"
        ))
    }

    pub(crate) async fn object_metadata(&self, key: &str) -> Result<Vec<String>> {
        let output = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        let mut fields = Vec::new();
        for (label, value) in [
            ("内容类型", output.content_type()),
            ("版本 ID", output.version_id()),
            ("缓存控制", output.cache_control()),
            (
                "服务端加密",
                output.server_side_encryption().map(|mode| mode.as_str()),
            ),
        ] {
            if let Some(value) = value {
                fields.push(format!("{label}：{value}"));
            }
        }
        if let Some(metadata) = output.metadata() {
            let mut entries: Vec<_> = metadata.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            fields.extend(
                entries
                    .into_iter()
                    .map(|(key, value)| format!("{key}：{value}")),
            );
        }
        Ok(fields)
    }

    pub(crate) async fn head(&self, key: &str) -> Result<ObjectTag> {
        let response = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| connection_error("无法读取对象 ETag", error))?;
        Ok(ObjectTag {
            value: response.e_tag().context("服务器未返回 ETag")?.to_owned(),
            size: response.content_length(),
            version: response.version_id().map(str::to_owned),
            md5_candidate: response.sse_customer_algorithm().is_none()
                && response
                    .server_side_encryption()
                    .is_none_or(|mode| mode.as_str() == "AES256"),
        })
    }
}

pub(crate) struct ObjectTag {
    value: String,
    pub(super) size: Option<i64>,
    version: Option<String>,
    md5_candidate: bool,
}

impl ObjectTag {
    pub(super) fn multipart(&self) -> Option<(&str, i32)> {
        let (digest, count) = self.value.trim_matches('"').split_once('-')?;
        let count = count.parse::<i32>().ok()?;
        (digest.len() == 32
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && (1..=10_000).contains(&count))
        .then_some((digest, count))
    }
    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    pub(super) fn matches_md5(&self, etag: &str, size: i64, md5: &str) -> bool {
        self.md5_candidate
            && self.size == Some(size)
            && self.value.trim_matches('"') == etag.trim_matches('"')
            && self.value.trim_matches('"').eq_ignore_ascii_case(md5)
    }

    async fn compare_progress(&self, path: &Path, progress: impl Fn(u64)) -> Result<String> {
        use md5::{Digest, Md5};
        let mut file = tokio::fs::File::open(path)
            .await
            .context("无法打开本地文件")?;
        let size = file.metadata().await?.len();
        if self
            .size
            .is_some_and(|expected| u64::try_from(expected) != Ok(size))
        {
            return Ok("文件大小不同".into());
        }
        let tag = self.value.trim_matches('"');
        if !self.md5_candidate
            || tag.len() != 32
            || !tag.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok("该 ETag 无法直接与整文件 MD5 比较。".into());
        }
        let mut hash = Md5::new();
        let mut received = 0;
        let mut buffer = vec![0; 256 * 1024];
        loop {
            let count = file.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            received += count as u64;
            progress((100 * received / size.max(1)).min(100));
        }
        let md5 = super::hex_digest(&hash.finalize());
        let result = if md5.eq_ignore_ascii_case(tag) {
            "本地 MD5 与 ETag 一致"
        } else {
            "本地 MD5 与 ETag 不一致"
        };
        Ok(format!("{result}\n本地 MD5：{md5}"))
    }
}

#[cfg(test)]
pub(super) async fn multipart_etag(
    reader: impl tokio::io::AsyncRead + Unpin,
    lengths: &[u64],
) -> Result<String> {
    multipart_etag_progress(reader, lengths, |_| {}).await
}

async fn multipart_etag_progress(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    lengths: &[u64],
    progress: impl Fn(u64),
) -> Result<String> {
    use md5::{Digest, Md5};
    let mut combined = Md5::new();
    let mut received = 0;
    let mut buffer = vec![0; 256 * 1024];
    for &length in lengths {
        let mut remaining = length;
        let mut part = Md5::new();
        while remaining > 0 {
            let chunk = remaining.min(buffer.len() as u64) as usize;
            reader
                .read_exact(&mut buffer[..chunk])
                .await
                .context("本地文件在比对期间发生变化或无法读取")?;
            part.update(&buffer[..chunk]);
            remaining -= chunk as u64;
            received += chunk as u64;
            progress(received);
        }
        combined.update(part.finalize());
    }
    ensure!(
        reader.read(&mut buffer[..1]).await? == 0,
        "本地文件在比对期间变长"
    );
    Ok(format!(
        "{}-{}",
        super::hex_digest(&combined.finalize()),
        lengths.len()
    ))
}
