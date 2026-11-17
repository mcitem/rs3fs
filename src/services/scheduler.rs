use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

pub(crate) const WAITING_FOR_PATHS: &str = "等待重叠路径的任务";

pub(crate) struct Scope {
    resource: Resource,
    write: bool,
}

enum Resource {
    Remote {
        endpoint: Option<String>,
        bucket: String,
        key: String,
        trash: bool,
    },
    Local(PathBuf),
}

impl Scope {
    pub(crate) fn remote(endpoint: Option<&str>, bucket: &str, key: &str, write: bool) -> Self {
        Self::remote_in(endpoint, bucket, key, write, false)
    }

    pub(crate) fn trash_record(endpoint: Option<&str>, bucket: &str, manifest: &str) -> Self {
        Self::remote_in(endpoint, bucket, manifest, true, true)
    }

    fn remote_in(
        endpoint: Option<&str>,
        bucket: &str,
        key: &str,
        write: bool,
        trash: bool,
    ) -> Self {
        Self {
            resource: Resource::Remote {
                endpoint: endpoint.map(endpoint_identity),
                bucket: bucket.to_owned(),
                key: key.to_owned(),
                trash,
            },
            write,
        }
    }

    pub(crate) fn local(path: &Path, write: bool) -> Self {
        let mut path = path
            .ancestors()
            .find_map(|ancestor| {
                let resolved = ancestor.canonicalize().ok()?;
                Some(resolved.join(path.strip_prefix(ancestor).ok()?))
            })
            .unwrap_or_else(|| path.to_owned());
        if cfg!(windows) {
            path = PathBuf::from(path.to_string_lossy().to_lowercase());
        }
        Self {
            resource: Resource::Local(path),
            write,
        }
    }

    fn conflicts(&self, other: &Self) -> bool {
        if !self.write && !other.write {
            return false;
        }
        match (&self.resource, &other.resource) {
            (
                Resource::Remote {
                    endpoint,
                    bucket,
                    key,
                    trash,
                },
                Resource::Remote {
                    endpoint: other_endpoint,
                    bucket: other_bucket,
                    key: other_key,
                    trash: other_trash,
                },
            ) => {
                endpoint == other_endpoint
                    && bucket == other_bucket
                    && trash == other_trash
                    && remote_keys_overlap(key, other_key)
            }
            (Resource::Local(path), Resource::Local(other)) => {
                path.starts_with(other) || other.starts_with(path)
            }
            _ => false,
        }
    }
}

fn endpoint_identity(endpoint: &str) -> String {
    match url::Url::parse(endpoint) {
        Ok(url) => url.as_str().trim_end_matches('/').to_owned(),
        Err(_) => endpoint.trim_end_matches('/').to_owned(),
    }
}

pub(crate) fn remote_keys_overlap(left: &str, right: &str) -> bool {
    left == right
        || ((left.is_empty() || left.ends_with('/')) && right.starts_with(left))
        || ((right.is_empty() || right.ends_with('/')) && left.starts_with(right))
}

#[derive(Clone, Default)]
pub(crate) struct Scheduler(Arc<State>);

#[derive(Default)]
struct State {
    queue: Mutex<Queue>,
    changed: watch::Sender<()>,
}

#[derive(Default)]
struct Queue {
    next_id: u64,
    requests: VecDeque<Request>,
}

struct Request {
    id: u64,
    scopes: Vec<Scope>,
}

pub(crate) struct Reservation {
    scheduler: Scheduler,
    id: u64,
}

impl Scheduler {
    pub(crate) async fn acquire(&self, scopes: Vec<Scope>) -> Reservation {
        let mut changed = self.0.changed.subscribe();
        let reservation = {
            let mut queue = self.0.queue.lock().unwrap();
            let id = queue.next_id;
            queue.next_id += 1;
            queue.requests.push_back(Request { id, scopes });
            Reservation {
                scheduler: self.clone(),
                id,
            }
        };
        loop {
            let ready = {
                let queue = self.0.queue.lock().unwrap();
                // Older conflicting requests get priority, while disjoint requests can pass.
                let mut earlier = queue.requests.iter().rev();
                let request = earlier
                    .find(|request| request.id == reservation.id)
                    .unwrap();
                !earlier.any(|item| {
                    item.scopes
                        .iter()
                        .any(|left| request.scopes.iter().any(|right| left.conflicts(right)))
                })
            };
            if ready {
                return reservation;
            }
            let _ = changed.changed().await;
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.scheduler
            .0
            .queue
            .lock()
            .unwrap()
            .requests
            .retain(|request| request.id != self.id);
        self.scheduler.0.changed.send_modify(|_| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;

    fn remote(bucket: &str, key: &str, write: bool) -> Scope {
        Scope::remote(Some("https://s3.example.test"), bucket, key, write)
    }

    #[tokio::test]
    async fn sync_blocks_overlapping_writes_and_leaves_other_deletes_runnable() {
        let scheduler = Scheduler::default();
        let sync = scheduler
            .acquire(vec![remote("bucket", "sync/", true)])
            .await;
        let mut inside = Box::pin(scheduler.acquire(vec![remote("bucket", "sync/photos/", true)]));
        assert!(poll!(&mut inside).is_pending());
        assert!(
            poll!(Box::pin(scheduler.acquire(vec![remote(
                "bucket",
                "sync-old/",
                true
            )])))
            .is_ready()
        );
        assert!(
            poll!(Box::pin(scheduler.acquire(vec![remote(
                "other-bucket",
                "sync/",
                true
            )])))
            .is_ready()
        );
        assert!(
            poll!(Box::pin(scheduler.acquire(vec![Scope::remote(
                Some("https://another.example.test"),
                "bucket",
                "sync/",
                true,
            )])))
            .is_ready()
        );
        let mut ancestor = Box::pin(scheduler.acquire(vec![remote("bucket", "", true)]));
        assert!(poll!(&mut ancestor).is_pending());
        drop(sync);
        let inside = inside.await;
        assert!(poll!(&mut ancestor).is_pending());
        drop(inside);
        let _ancestor = ancestor.await;
    }

    #[tokio::test]
    async fn queued_writer_blocks_later_readers_until_cancelled() {
        let scheduler = Scheduler::default();
        let read = scheduler
            .acquire(vec![remote("bucket", "sync/", false)])
            .await;
        assert!(
            poll!(Box::pin(scheduler.acquire(vec![remote(
                "bucket",
                "sync/file",
                false
            )])))
            .is_ready()
        );
        let mut writer = Box::pin(scheduler.acquire(vec![Scope::remote(
            Some("https://S3.EXAMPLE.TEST:443/"),
            "bucket",
            "sync/file",
            true,
        )]));
        assert!(poll!(&mut writer).is_pending());
        let mut later = Box::pin(scheduler.acquire(vec![remote("bucket", "sync/file", false)]));
        assert!(poll!(&mut later).is_pending());
        drop(writer);
        assert!(poll!(&mut later).is_ready());
        drop(read);
    }

    #[tokio::test]
    async fn trash_maintenance_does_not_wait_for_live_bucket_root() {
        let scheduler = Scheduler::default();
        let endpoint = Some("https://s3.example.test");
        let _sync = scheduler.acquire(vec![remote("bucket", "", true)]).await;
        let trash = || Scope::trash_record(endpoint, "bucket", "manifest");
        let std::task::Poll::Ready(purge) = poll!(Box::pin(scheduler.acquire(vec![trash()])))
        else {
            panic!("recycle-bin maintenance must bypass live objects");
        };
        let mut restore = Box::pin(scheduler.acquire(vec![trash()]));
        assert!(poll!(&mut restore).is_pending());
        drop(purge);
        assert!(poll!(&mut restore).is_ready());
    }

    #[tokio::test]
    async fn sync_reserves_local_and_remote_paths_together() -> anyhow::Result<()> {
        let scheduler = Scheduler::default();
        let directory = tempfile::tempdir()?;
        let sync_folder = directory.path().join("sync");
        std::fs::create_dir(&sync_folder)?;
        let sync = scheduler
            .acquire(vec![
                Scope::local(&sync_folder, true),
                remote("bucket", "sync/", false),
            ])
            .await;
        let mut upload = Box::pin(scheduler.acquire(vec![
            Scope::local(&sync_folder.join("upload.txt"), false),
            remote("other", "upload.txt", true),
        ]));
        assert!(poll!(&mut upload).is_pending());
        assert!(
            poll!(Box::pin(scheduler.acquire(vec![
                Scope::local(
                    &directory.path().join("sync-old").join("download.txt"),
                    true
                ),
                remote("bucket", "sync/download.txt", false),
            ])))
            .is_ready()
        );
        drop(sync);
        let _upload = upload.await;
        Ok(())
    }
}
