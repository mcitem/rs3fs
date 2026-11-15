use super::{Entry, Store, connection_error, trash};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::types::EncodingType;
use percent_encoding::percent_decode_str;

impl Store {
    pub(crate) fn sync_key_allowed(key: &str) -> bool {
        !trash::reserved(key) && key.len() <= 1024
    }

    pub(crate) async fn usage(&self) -> Result<i64> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .into_paginator()
            .send();
        let mut bytes = 0i64;
        while let Some(page) = pages.next().await {
            let page = page.map_err(|error| {
                connection_error(&format!("无法统计 {} 的用量", self.bucket), error)
            })?;
            for object in page.contents() {
                let size = object.size().context("S3 未返回对象大小")?;
                ensure!(size >= 0, "S3 返回了无效的对象大小");
                bytes = bytes.checked_add(size).context("存储桶用量超出统计范围")?;
            }
        }
        Ok(bytes)
    }

    pub(crate) async fn list(&self, prefix: &str, immediate: bool) -> Result<Vec<Entry>> {
        ensure!(!trash::reserved(prefix), "此路径由回收站保留。");
        let mut entries = self.list_objects(prefix, immediate).await?;
        entries.retain(|entry| !trash::reserved(&entry.key));
        Ok(entries)
    }

    pub(super) async fn list_objects(&self, prefix: &str, immediate: bool) -> Result<Vec<Entry>> {
        match self
            .list_objects_with_encoding(prefix, immediate, true)
            .await?
        {
            Some(entries) => Ok(entries),
            None => self
                .list_objects_with_encoding(prefix, immediate, false)
                .await?
                .context("S3 返回了无法区分空格与加号的对象键"),
        }
    }

    async fn list_objects_with_encoding(
        &self,
        prefix: &str,
        immediate: bool,
        request_encoding: bool,
    ) -> Result<Option<Vec<Entry>>> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .set_delimiter(immediate.then(|| "/".to_string()))
            .set_encoding_type(request_encoding.then_some(EncodingType::Url))
            .into_paginator()
            .send();
        let mut entries = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|error| {
                connection_error(&format!("无法列出 {} /{prefix}", self.bucket), error)
            })?;
            let encoded = page.encoding_type() == Some(&EncodingType::Url);
            // Some compatible endpoints apply form encoding, where a raw plus
            // represents a space. Ask for literal keys when the result is ambiguous.
            if encoded
                && (page
                    .contents()
                    .iter()
                    .filter_map(|object| object.key())
                    .any(|key| key.contains('+'))
                    || page
                        .common_prefixes()
                        .iter()
                        .filter_map(|folder| folder.prefix())
                        .any(|key| key.contains('+')))
            {
                return Ok(None);
            }
            let decode = |key: &str| -> Result<String> {
                if encoded {
                    Ok(percent_decode_str(key).decode_utf8()?.into_owned())
                } else {
                    Ok(key.to_owned())
                }
            };
            for folder in page.common_prefixes() {
                if let Some(key) = folder.prefix() {
                    entries.push(Entry::folder(decode(key)?));
                }
            }
            for object in page.contents() {
                let Some(key) = object.key() else { continue };
                let key = decode(key)?;
                if immediate && key == prefix {
                    continue;
                }
                entries.push(Entry {
                    key,
                    directory: false,
                    size: object.size().unwrap_or_default(),
                    etag: object.e_tag().map(str::to_owned),
                    modified: object.last_modified().and_then(|date| {
                        chrono::DateTime::from_timestamp(date.secs(), date.subsec_nanos())
                    }),
                    storage_class: object
                        .storage_class()
                        .map(|class| class.as_str().to_owned()),
                });
            }
        }
        if immediate {
            entries.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.key.cmp(&b.key))
            });
        }
        Ok(Some(entries))
    }
}
