use super::{Entry, Store, connection_error, trash};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::types::EncodingType;
use futures_util::{StreamExt, TryStreamExt, stream};
use percent_encoding::percent_decode_str;

impl Store {
    pub(crate) async fn sync_retry_entry(&self, key: &str) -> Result<Option<Entry>> {
        if !key.ends_with('/') {
            let descendants = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(format!("{key}/"))
                .max_keys(1)
                .send()
                .await
                .map_err(|error| super::connection_error("无法检查同步路径", error))?;
            anyhow::ensure!(
                descendants.contents().is_empty(),
                "S3 路径已有同名目录：/{key}"
            );
        } else {
            anyhow::ensure!(
                self.recovery_head(key.trim_end_matches('/'))
                    .await?
                    .is_none(),
                "S3 路径已有同名文件：/{key}"
            );
        }
        self.head_entry(key).await
    }

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
        self.collect_listing(prefix, immediate, false)
            .await
            .map(|(entries, _)| entries)
    }

    pub(crate) async fn list_sync_tree(&self, prefix: &str, workers: usize) -> Result<Vec<Entry>> {
        ensure!(!trash::reserved(prefix), "此路径由回收站保留。");
        let mut files = Vec::new();
        let mut parent = prefix.to_owned();
        let mut depth = 0;
        let branches = loop {
            let (entries, truncated) = self.collect_listing(&parent, true, true).await?;
            let (mut branches, objects): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .filter(|entry| !trash::reserved(entry.key()))
                .partition(Entry::directory);
            // A capped probe avoids listing once per directory in a flat or narrow tree.
            if truncated || branches.len() > 32 {
                files.extend(self.list(&parent, false).await?);
                return Ok(files);
            }
            for branch in &branches {
                let child = branch.key().strip_prefix(&parent);
                ensure!(
                    child
                        .and_then(|child| child.strip_suffix('/'))
                        .is_some_and(|child| !child.is_empty() && !child.contains('/')),
                    "S3 返回了无效的子目录：{}",
                    branch.key()
                );
            }
            files.extend(objects);
            if branches.len() != 1 || depth >= 3 {
                break branches;
            }
            parent = branches.pop().unwrap().key;
            depth += 1;
        };
        let mut listings = stream::iter(branches)
            .map(|branch| async move {
                let listing = self.list(branch.key(), false).await?;
                ensure!(
                    listing
                        .iter()
                        .all(|entry| entry.key().starts_with(branch.key())),
                    "S3 返回了同步子目录之外的对象：{}",
                    branch.key()
                );
                Ok::<_, anyhow::Error>(listing)
            })
            .buffer_unordered(workers);
        while let Some(mut listing) = listings.try_next().await? {
            files.append(&mut listing);
        }
        Ok(files)
    }

    async fn collect_listing(
        &self,
        prefix: &str,
        immediate: bool,
        first_page: bool,
    ) -> Result<(Vec<Entry>, bool)> {
        match self
            .list_objects_with_encoding(prefix, immediate, true, first_page)
            .await?
        {
            Some(entries) => Ok(entries),
            None => self
                .list_objects_with_encoding(prefix, immediate, false, first_page)
                .await?
                .context("S3 返回了无法区分空格与加号的对象键"),
        }
    }

    async fn list_objects_with_encoding(
        &self,
        prefix: &str,
        immediate: bool,
        request_encoding: bool,
        first_page: bool,
    ) -> Result<Option<(Vec<Entry>, bool)>> {
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
        let mut truncated = false;
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
                if immediate && !first_page && key == prefix {
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
            if first_page {
                truncated = page.is_truncated().unwrap_or(false)
                    || page.next_continuation_token().is_some();
                break;
            }
        }
        if immediate {
            entries.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.key.cmp(&b.key))
            });
        }
        Ok(Some((entries, truncated)))
    }
}
