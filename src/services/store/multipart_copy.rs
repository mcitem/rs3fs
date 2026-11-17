use super::{COPY_LIMIT, Entry, MIB, Store, TransferCancelled, connection_error, upload};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::{
    error::{ProvideErrorMetadata, SdkError},
    operation::head_object::HeadObjectOutput,
};
use tokio_util::sync::CancellationToken;

pub(super) struct CopyMetadata {
    pub(super) original: HeadObjectOutput,
    pub(super) tags: Option<String>,
}

macro_rules! apply_copy_metadata {
    ($request:expr, $metadata:expr) => {{
        let metadata = $metadata;
        let original = &metadata.original;
        $request
            .set_metadata(original.metadata().cloned())
            .set_storage_class(original.storage_class().cloned())
            .set_content_type(original.content_type().map(str::to_owned))
            .set_content_disposition(original.content_disposition().map(str::to_owned))
            .set_content_encoding(original.content_encoding().map(str::to_owned))
            .set_content_language(original.content_language().map(str::to_owned))
            .set_cache_control(original.cache_control().map(str::to_owned))
            .set_expires(original.expires_string().and_then(|expires| {
                aws_smithy_types::DateTime::from_str(
                    expires,
                    aws_smithy_types::date_time::Format::HttpDate,
                )
                .ok()
            }))
            .set_tagging(metadata.tags.clone())
    }};
}
pub(super) use apply_copy_metadata;

#[derive(Debug)]
pub(super) struct ServerCopyUnsupported(anyhow::Error);

impl std::fmt::Display for ServerCopyUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ServerCopyUnsupported {}

pub(super) fn copy_error<E: ProvideErrorMetadata + std::fmt::Debug>(
    action: &str,
    error: SdkError<E>,
) -> anyhow::Error {
    let unsupported = matches!(
        error.code(),
        Some("NotImplemented" | "UnsupportedOperation" | "EntityTooLarge")
    ) || error
        .raw_response()
        .is_some_and(|response| matches!(response.status().as_u16(), 405 | 501));
    let error = connection_error(action, error);
    if unsupported {
        ServerCopyUnsupported(error).into()
    } else {
        error
    }
}

impl Store {
    pub(super) async fn copy_metadata(&self, object: &Entry, etag: &str) -> Result<CopyMetadata> {
        let original = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(object.key())
            .if_match(etag)
            .send()
            .await
            .map_err(|error| connection_error("无法读取分片复制源对象", error))?;
        ensure!(
            original.e_tag() == Some(etag) && original.content_length() == Some(object.size()),
            "分片复制源对象已变化：/{}",
            object.key()
        );
        let tags = if original.tag_count().is_some_and(|count| count > 0) {
            self.object_tags(object.key(), &original).await?
        } else {
            None
        };
        Ok(CopyMetadata { original, tags })
    }

    pub(super) async fn object_tags(
        &self,
        key: &str,
        original: &HeadObjectOutput,
    ) -> Result<Option<String>> {
        if original.tag_count() == Some(0) {
            return Ok(None);
        }
        let response = self
            .client
            .get_object_tagging()
            .bucket(&self.bucket)
            .key(key)
            .set_version_id(original.version_id().map(str::to_owned))
            .send()
            .await;
        let tags = match response {
            Ok(tags) => tags,
            Err(error)
                if original.tag_count().is_none()
                    && (error.code() == Some("NotImplemented")
                        || error.raw_response().is_some_and(|response| {
                            matches!(response.status().as_u16(), 405 | 501)
                        })) =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(connection_error(
                    &format!("无法读取 /{key} 的对象标签"),
                    error,
                ));
            }
        };
        Ok((!tags.tag_set().is_empty()).then(|| {
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(tags.tag_set().iter().map(|tag| (tag.key(), tag.value())))
                .finish()
        }))
    }

    pub(super) async fn copy_multipart(
        &self,
        target: &Store,
        object: &Entry,
        key: &str,
        source: &str,
        etag: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let size = u64::try_from(object.size())?;
        let part_size = upload::upload_part_size(size).max(128 * MIB);
        ensure!(part_size <= COPY_LIMIT as u64, "对象超过分片复制上限");
        ensure!(
            target.recovery_head(key).await?.is_none(),
            "目标对象已存在：/{key}"
        );

        let metadata = self.copy_metadata(object, etag).await?;
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        let request = target
            .client
            .create_multipart_upload()
            .bucket(&target.bucket)
            .key(key);
        let upload = apply_copy_metadata!(request, &metadata)
            .send()
            .await
            .map_err(|error| copy_error(&format!("无法创建分片复制 /{key}"), error))?;
        let id = upload.upload_id().context("服务端未返回分片上传 ID")?;

        let result = async {
            let count = size.div_ceil(part_size);
            let mut parts = Vec::with_capacity(count as usize);
            for index in 0..count {
                ensure!(!cancel.is_cancelled(), TransferCancelled);
                let first = index * part_size;
                let last = (first + part_size).min(size) - 1;
                let number = (index + 1) as i32;
                let copied = target
                    .client
                    .upload_part_copy()
                    .bucket(&target.bucket)
                    .key(key)
                    .upload_id(id)
                    .part_number(number)
                    .copy_source(source)
                    .copy_source_if_match(etag)
                    .copy_source_range(format!("bytes={first}-{last}"))
                    .send()
                    .await
                    .map_err(|error| {
                        copy_error(
                            &format!("无法复制 /{key} 的分片 {number}/{count}（UploadPartCopy）"),
                            error,
                        )
                    })?;
                let part_etag = copied
                    .copy_part_result()
                    .and_then(|part| part.e_tag())
                    .context("服务端未返回复制分片的 ETag")?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(number)
                        .e_tag(part_etag)
                        .build(),
                );
            }
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            target
                .client
                .complete_multipart_upload()
                .bucket(&target.bucket)
                .key(key)
                .upload_id(id)
                .if_none_match("*")
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .map_err(|error| connection_error(&format!("无法合并 /{key} 的复制分片"), error))
        }
        .await;
        let completed = match result {
            Ok(completed) => completed,
            Err(error) => {
                let abort = target
                    .client
                    .abort_multipart_upload()
                    .bucket(&target.bucket)
                    .key(key)
                    .upload_id(id)
                    .send()
                    .await;
                if let Err(abort) = abort
                    && !abort
                        .raw_response()
                        .is_some_and(|response| response.status().as_u16() == 404)
                {
                    let cleanup = connection_error("清理复制分片失败", abort);
                    return Err(anyhow::anyhow!("{error:#}。{cleanup}"));
                }
                return if cancel.is_cancelled() {
                    Err(TransferCancelled.into())
                } else {
                    Err(error)
                };
            }
        };
        let expected = completed
            .e_tag()
            .context("服务端未返回分片复制结果的 ETag")?;
        let actual = target.recovery_head(key).await?;
        ensure!(
            actual.is_some_and(|(etag, size)| etag == expected && size == object.size()),
            "分片复制后的目标对象无法核对：/{key}"
        );
        Ok(expected.to_owned())
    }
}
