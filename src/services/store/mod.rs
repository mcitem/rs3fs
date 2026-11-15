use aws_sdk_s3::Client;

mod connection;
mod download;
mod durable_transfer;
mod etag;
mod listing;
mod mutations;
mod paths;
mod trash;
mod upload;

pub(crate) use connection::{Bucket, Server, ServerCapacity};
pub(crate) use download::{TextEncoding, decode_editable_text};
pub(crate) use durable_transfer::local_path;
#[cfg(test)]
pub(crate) use mutations::RelocateProgress;
pub(crate) use paths::{child_key, parent_prefix, upload_plan};
pub(crate) use trash::TrashEntry;
pub(crate) use upload::{TransferCancelled, TransferSlots, UploadTransfer};

use connection::connection_error;

const COPY_LIMIT: i64 = 5 * 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[derive(Clone)]
pub(crate) struct Store {
    client: Client,
    bucket: String,
    transfer_slots: TransferSlots,
    #[cfg(windows)]
    preview_endpoint: Option<String>,
    osca_quota_url: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct Entry {
    key: String,
    directory: bool,
    size: i64,
    etag: Option<String>,
    modified: Option<chrono::DateTime<chrono::Utc>>,
    storage_class: Option<String>,
}

impl Entry {
    pub(crate) fn update_after_write(&mut self, size: i64, etag: Option<String>) {
        self.size = size;
        self.etag = etag;
        self.modified = Some(chrono::Utc::now());
    }

    pub(crate) fn file(key: String, size: i64, etag: String) -> Self {
        Self {
            key,
            directory: false,
            size,
            etag: Some(etag),
            modified: None,
            storage_class: None,
        }
    }

    pub(crate) fn folder(key: String) -> Self {
        Self {
            key,
            directory: true,
            size: 0,
            etag: None,
            modified: None,
            storage_class: None,
        }
    }
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
    pub(crate) fn directory(&self) -> bool {
        self.directory
    }
    pub(crate) fn name(&self) -> &str {
        let key = if self.directory {
            self.key.strip_suffix('/').unwrap_or(&self.key)
        } else {
            &self.key
        };
        let name = key.rsplit('/').next().unwrap_or(key);
        if name.is_empty() { "/" } else { name }
    }
    pub(crate) fn modified(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.modified
    }

    pub(crate) fn storage_class(&self) -> Option<&str> {
        self.storage_class.as_deref()
    }

    pub(crate) fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }
    pub(crate) fn size(&self) -> i64 {
        self.size
    }
}

#[cfg(test)]
mod tests;
