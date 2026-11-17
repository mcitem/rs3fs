use super::{
    config::read,
    local::{LocalFile, scan_local, valid_component},
    plan::{build_plan, decide},
    *,
};
use std::sync::{Arc, atomic::AtomicU64};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable loopback S3 endpoint"]
async fn loopback_parallel_sync_resumes_inside_deep_branches() -> Result<()> {
    let (store, bucket) = loopback_store().await?;
    let temp = tempfile::tempdir()?;
    let folder = temp.path().join("local");
    std::fs::create_dir(&folder)?;
    let snapshot = temp.path().join("sync.json");
    let job = SyncJob {
        id: Uuid::new_v4(),
        profile: Uuid::new_v4(),
        bucket,
        bucket_region: None,
        prefix: format!("{}/", Uuid::new_v4()),
        folder,
        direction: Direction::Both,

        automatic: false,
        interval_minutes: 5,
        watch_local: false,
    };
    let paths = (0..32)
        .map(|index| {
            let branch = if index % 2 == 0 { "甲" } else { "B" };
            format!("nested/parent/{branch}/deep/shared/{index}+file.txt")
        })
        .collect::<Vec<_>>();
    let source = temp.path().join("source.txt");
    std::fs::write(&source, b"test")?;
    for path in &paths {
        store
            .upload_file(
                &format!("{}{path}", job.prefix),
                &source,
                4,
                Arc::new(AtomicU64::new(0)),
                None,
            )
            .await?;
    }
    store
        .mkdir(format!("{}nested/parent/empty/", job.prefix))
        .await?;
    for path in paths.iter().take(5) {
        let local = job.folder.join(path);
        std::fs::create_dir_all(local.parent().unwrap())?;
        std::fs::write(local, b"test")?;
    }
    let slots = TransferSlots::new(4);
    let cancel = CancellationToken::new();
    let plan = build_plan(&store, job.clone(), &snapshot, &slots, &cancel, &mut |_| {}).await?;
    assert_eq!(
        plan.items
            .iter()
            .filter(|item| item.action == Action::Match)
            .count(),
        5
    );
    assert_eq!(
        plan.items
            .iter()
            .filter(|item| item.action == Action::Download)
            .count(),
        27
    );
    let rebuilt = build_plan(&store, job.clone(), &snapshot, &slots, &cancel, &mut |_| {}).await?;
    plan.verify(&rebuilt)?;
    let result = execute_plan(
        &store,
        plan,
        &snapshot,
        &BTreeMap::new(),
        &slots,
        &cancel,
        &mut |progress| {
            if progress.downloaded >= 3 {
                cancel.cancel();
            }
        },
    )
    .await;
    assert!(result.unwrap_err().is::<TransferCancelled>());
    let state = baseline::load(&snapshot)?;
    let completed = paths
        .iter()
        .filter(|path| job.folder.join(path).is_file())
        .count();
    assert_eq!(
        completed - 5,
        state.keys().filter(|key| !paths[..5].contains(key)).count()
    );
    assert!((8..32).contains(&completed));
    for path in state.keys() {
        assert_eq!(std::fs::read(job.folder.join(path))?, b"test");
    }
    let cancel = CancellationToken::new();
    let plan = build_plan(&store, job.clone(), &snapshot, &slots, &cancel, &mut |_| {}).await?;
    execute_plan(
        &store,
        plan,
        &snapshot,
        &BTreeMap::new(),
        &slots,
        &cancel,
        &mut |_| {},
    )
    .await?;
    assert_eq!(baseline::load(&snapshot)?.len(), paths.len());
    for path in paths {
        assert_eq!(std::fs::read(job.folder.join(path))?, b"test");
    }
    assert!(job.folder.join("nested/parent/empty").is_dir());
    Ok(())
}

#[test]
#[ignore = "requires a disposable loopback S3 endpoint"]
fn loopback_sync_and_capacity() -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let (store, bucket) = loopback_store().await?;
        let temp = tempfile::tempdir()?;
        let folder = temp.path().join("local");
        std::fs::create_dir(&folder)?;
        let local = folder.join("file.txt");
        std::fs::write(&local, b"one")?;
        let snapshot = temp.path().join("state.json");
        let job = SyncJob {
            id: Uuid::new_v4(),
            profile: Uuid::new_v4(),
            bucket,
            bucket_region: None,
            prefix: format!("{}/", Uuid::new_v4()),
            folder,
            direction: Direction::Both,

            automatic: false,
            interval_minutes: 5,
            watch_local: false,
        };
        let key = format!("{}file.txt", job.prefix);
        std::fs::create_dir_all(job.folder.join("local-empty/nested"))?;
        store.mkdir(format!("{}remote-empty/", job.prefix)).await?;
        let baseline = store.usage().await?;
        assert!(
            run_with_snapshot(store.clone(), job.clone(), snapshot.clone())
                .await?
                .contains("上传 1")
        );
        assert!(job.folder.join("remote-empty").is_dir());
        assert!(
            store
                .list(&job.prefix, false)
                .await?
                .iter()
                .any(|object| object.key() == format!("{}local-empty/nested/", job.prefix))
        );
        assert_eq!(store.usage().await?, baseline + 3);
        std::fs::write(&local, b"two!")?;
        assert!(
            run_with_snapshot(store.clone(), job.clone(), snapshot.clone())
                .await
                .unwrap_err()
                .is::<SyncConflict>()
        );
        assert_eq!(store.preview_text(&key, 100).await?, "one");

        let upload = SyncJob {
            direction: Direction::Upload,
            ..job.clone()
        };
        assert!(
            run_with_snapshot(store.clone(), upload, snapshot.clone())
                .await?
                .contains("上传 1")
        );
        assert_eq!(store.preview_text(&key, 100).await?, "two!");
        assert!(
            store
                .list_trash()
                .await?
                .iter()
                .any(|item| item.original() == key)
        );

        std::fs::write(&local, b"new local")?;
        let download = SyncJob {
            direction: Direction::Download,
            ..job.clone()
        };
        assert!(
            run_with_snapshot(store.clone(), download, snapshot.clone())
                .await?
                .contains("下载 1")
        );
        assert_eq!(std::fs::read(&local)?, b"two!");

        let review_job = SyncJob {
            prefix: format!("{}/", Uuid::new_v4()),
            folder: temp.path().join("review"),

            ..job
        };
        std::fs::create_dir(&review_job.folder)?;
        let review_file = review_job.folder.join("review.txt");
        let review_state = temp.path().join("review-state.json");
        let review_key = format!("{}review.txt", review_job.prefix);
        let slots = TransferSlots::new(4);
        let cancel = CancellationToken::new();
        std::fs::write(&review_file, b"review")?;
        let build = || async {
            build_plan(
                &store,
                review_job.clone(),
                &review_state,
                &slots,
                &cancel,
                &mut ignore_progress,
            )
            .await
        };
        let original = build().await?;
        assert_eq!(original.items()[0].label(), "上传");
        assert!(!review_state.exists());
        assert!(store.list(&review_job.prefix, false).await?.is_empty());

        std::fs::write(&review_file, b"changed")?;
        assert!(original.verify(&build().await?).is_err());
        assert!(store.list(&review_job.prefix, false).await?.is_empty());
        assert!(
            execute_plan(
                &store,
                original,
                &review_state,
                &BTreeMap::new(),
                &slots,
                &cancel,
                &mut ignore_progress
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&review_file)?, b"changed");
        assert!(!review_state.exists());

        let plan = build().await?;
        execute_plan(
            &store,
            plan,
            &review_state,
            &BTreeMap::new(),
            &slots,
            &cancel,
            &mut ignore_progress,
        )
        .await?;
        let state_before_preview = std::fs::read(&review_state)?;
        let etag = store.head(&review_key).await?;
        store.delete_if_unchanged(&review_key, etag.value()).await?;
        let restore_plan = build().await?;
        assert_eq!(restore_plan.items()[0].label(), "上传");
        assert_eq!(std::fs::read(&review_state)?, state_before_preview);
        store.create_file(review_key.clone()).await?;
        assert!(restore_plan.verify(&build().await?).is_err());

        let reserved_job = SyncJob {
            prefix: format!("{}/", Uuid::new_v4()),
            folder: temp.path().to_owned(),
            ..review_job
        };
        let reserved_key = format!("{}sync-recovery/foreign.txt", reserved_job.prefix);
        store.create_file(reserved_key.clone()).await?;
        let plan = build_plan(
            &store,
            reserved_job,
            &review_state,
            &slots,
            &cancel,
            &mut ignore_progress,
        )
        .await?;
        assert!(
            plan.items()
                .iter()
                .all(|item| !item.path().starts_with("sync-recovery"))
        );
        assert!(store.head(&reserved_key).await.is_ok());
        let url = url::Url::parse(&store.share_url(key, 15).await?)?;
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "X-Amz-Expires" && value == "900")
        );
        Ok(())
    })
}

#[test]
fn sync_scan_skips_office_lock_files_and_download_partials() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::create_dir(temp.path().join("docs"))?;
    std::fs::write(temp.path().join("docs/~$report.docx"), b"lock")?;
    let partial = format!("docs/.s3fs-{}-0.part", Uuid::new_v4());
    std::fs::write(temp.path().join(partial), b"partial download")?;
    std::fs::write(
        temp.path().join("docs/.s3fs-project.part"),
        b"ordinary file",
    )?;
    std::fs::write(temp.path().join("docs/report.docx"), b"document")?;
    let tree = scan_local(temp.path())?;
    assert_eq!(
        tree.files.keys().map(String::as_str).collect::<Vec<_>>(),
        ["docs/.s3fs-project.part", "docs/report.docx"]
    );
    Ok(())
}

#[test]
fn directions_use_authoritative_source_or_report_same_path_differences() {
    use Action::{Conflict, Download, Keep, Upload};
    let local = LocalFile {
        bytes: 1,
        digest: [0; 16],
    };
    let changed = LocalFile {
        bytes: 2,
        digest: [1; 16],
    };
    let remote = RemoteFile {
        bytes: 1,
        etag: "first".into(),
    };
    let remote_changed = RemoteFile {
        bytes: 2,
        etag: "second".into(),
    };
    let pair = Pair {
        local: local.clone(),
        remote: remote.clone(),
    };
    for direction in [Direction::Upload, Direction::Download, Direction::Both] {
        assert_eq!(
            decide(direction, Some(&pair), Some(&local), Some(&remote)),
            Keep
        );
    }
    for (direction, both) in [
        (Direction::Upload, Upload),
        (Direction::Download, Download),
        (Direction::Both, Conflict),
    ] {
        assert_eq!(decide(direction, None, Some(&local), Some(&remote)), both);
        assert_eq!(
            decide(direction, Some(&pair), Some(&changed), Some(&remote)),
            both
        );
        assert_eq!(
            decide(direction, Some(&pair), Some(&local), Some(&remote_changed)),
            both
        );
    }
    assert_eq!(
        decide(Direction::Upload, Some(&pair), None, Some(&remote)),
        Keep
    );
    assert_eq!(
        decide(Direction::Download, Some(&pair), Some(&local), None),
        Keep
    );
    assert_eq!(
        decide(Direction::Both, Some(&pair), None, Some(&remote)),
        Download
    );
    assert_eq!(
        decide(Direction::Both, Some(&pair), Some(&local), None),
        Upload
    );
    for path in ["..", "C:", "file:stream", "CON.txt", "COM¹.txt", "a."] {
        assert!(!valid_component(path), "{path}");
    }
}

#[tokio::test]
#[ignore = "requires a disposable loopback S3 endpoint"]
async fn loopback_applies_mixed_conflict_choices_and_skips_a_stale_version() -> Result<()> {
    let (store, bucket) = loopback_store().await?;
    let temp = tempfile::tempdir()?;
    let folder = temp.path().join("local");
    std::fs::create_dir(&folder)?;
    let snapshot = temp.path().join("state.json");
    let job = SyncJob {
        id: Uuid::new_v4(),
        profile: Uuid::new_v4(),
        bucket,
        bucket_region: None,
        prefix: format!("{}/", Uuid::new_v4()),
        folder,
        direction: Direction::Both,
        automatic: false,
        interval_minutes: 5,
        watch_local: false,
    };
    let source = temp.path().join("source.txt");
    std::fs::write(&source, b"S3 data")?;
    for path in ["local.txt", "remote.txt", "stale.txt"] {
        std::fs::write(job.folder.join(path), b"local")?;
        store
            .upload_file(
                &format!("{}{path}", job.prefix),
                &source,
                7,
                Arc::new(AtomicU64::new(0)),
                None,
            )
            .await?;
    }
    let slots = TransferSlots::new(2);
    let cancel = CancellationToken::new();
    let scan = || async {
        build_plan(
            &store,
            job.clone(),
            &snapshot,
            &slots,
            &cancel,
            &mut ignore_progress,
        )
        .await
    };
    let shown = scan().await?;
    assert_eq!(shown.items().len(), 3);
    assert!(shown.items().iter().all(|item| item.is_conflict()
        && item.modified(ConflictChoice::Local).is_some()
        && item.modified(ConflictChoice::Remote).is_some()));

    let selections = shown
        .items()
        .iter()
        .map(|item| {
            let choice = if item.path() == "remote.txt" {
                ConflictChoice::Remote
            } else {
                ConflictChoice::Local
            };
            (item.path().to_owned(), (item.clone(), choice))
        })
        .collect();
    std::fs::write(job.folder.join("stale.txt"), b"changed")?;
    let mut current = scan().await?;
    let accepted = current.prepare_conflict_choices(&selections);
    assert_eq!(accepted.len(), 2);
    assert!(!accepted.contains_key("stale.txt"));
    execute_plan(
        &store,
        current,
        &snapshot,
        &accepted,
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await?;

    assert_eq!(
        store
            .preview_text(&format!("{}local.txt", job.prefix), 100)
            .await?,
        "local"
    );
    assert_eq!(std::fs::read(job.folder.join("remote.txt"))?, b"S3 data");
    assert_eq!(std::fs::read(job.folder.join("stale.txt"))?, b"changed");
    assert_eq!(
        store
            .preview_text(&format!("{}stale.txt", job.prefix), 100)
            .await?,
        "S3 data"
    );
    let remaining = scan().await?;
    assert_eq!(remaining.items().len(), 1);
    assert_eq!(remaining.items()[0].path(), "stale.txt");
    Ok(())
}

fn ignore_progress(_: &SyncProgress) {}

#[test]
fn sync_recovery_failure_retains_source_and_excludes_backups_from_scanning() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let state = temp.path().join("state.json");
    let root = recovery::path(&state)?;
    let original = temp.path().join("original.txt");
    std::fs::write(&original, b"original")?;
    let expected = local_file(&original)?;
    std::fs::write(&original, b"modified")?;
    assert!(recovery::archive(&root, Uuid::new_v4(), &original, &expected).is_err());
    assert_eq!(std::fs::read(&original)?, b"modified");
    assert_eq!(std::fs::read_dir(&root)?.count(), 0);

    let expected = local_file(&original)?;
    recovery::archive(&root, Uuid::new_v4(), &original, &expected)?;
    let relative = local::recovery_relative(temp.path(), &root)?;
    let scanned = local::scan_local_excluding(temp.path(), relative.as_deref(), None)?;
    assert_eq!(scanned.files.len(), 1);
    assert!(scanned.files.contains_key("original.txt"));
    assert!(local::recovery_relative(&root, &root).is_err());
    Ok(())
}

#[test]
fn sync_scope_blocks_reusing_a_baseline_for_a_different_endpoint() -> Result<()> {
    use crate::services::settings::ConnectionSettings;
    let temp = tempfile::tempdir()?;
    let snapshot = temp.path().join("sync.json");
    let job = SyncJob {
        id: Uuid::new_v4(),
        profile: Uuid::new_v4(),
        bucket: "fixture-bucket".into(),
        bucket_region: None,
        prefix: "project/".into(),
        folder: temp.path().to_owned(),
        direction: Direction::Both,

        automatic: true,
        interval_minutes: 5,
        watch_local: false,
    };
    let connection = |endpoint: &str| {
        ConnectionSettings::new(
            String::new(),
            "us-east-1".into(),
            endpoint.into(),
            String::new(),
        )
    };
    let old = config::Scope::new(&job, &connection("https://old.example"))?;
    assert!(!old.verify(&snapshot)?);
    assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
    old.bind(&snapshot)?;
    assert!(old.verify(&snapshot)?);
    let changed = config::Scope::new(&job, &connection("https://new.example"))?;
    assert!(changed.bind(&snapshot).is_err());
    assert!(old.verify(&snapshot)?);
    Ok(())
}

#[test]
fn cancelled_scan_leaves_the_local_folder_untouched() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("sample.txt"), b"sample")?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let Err(error) = local::scan_local_excluding(temp.path(), None, Some(&cancel)) else {
        anyhow::bail!("已取消的扫描仍然完成");
    };
    assert!(error.is::<TransferCancelled>());
    assert_eq!(std::fs::read(temp.path().join("sample.txt"))?, b"sample");
    Ok(())
}

async fn loopback_store() -> Result<(Store, String)> {
    let endpoint = std::env::var("S3FS_TEST_ENDPOINT")?;
    let bucket = std::env::var("S3FS_TEST_BUCKET")?;
    ensure!(
        endpoint.starts_with("http://127.0.0.1:"),
        "测试端点必须在本机"
    );
    let credentials =
        aws_sdk_s3::config::Credentials::new("local-fixture", "local-fixture", None, None, "test");
    let server = crate::services::store::Server::connect(
        "us-east-1".into(),
        endpoint,
        String::new(),
        Some(credentials),
    )
    .await?;
    Ok((server.open(bucket.clone(), None), bucket))
}

#[cfg(windows)]
#[test]
#[ignore = "requires a disposable loopback S3 endpoint"]
fn loopback_sync_waits_for_locked_files() -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let (store, bucket) = loopback_store().await?;
        let temp = tempfile::tempdir()?;
        assert_sync_waits_for_locked_files(&store, bucket, temp.path()).await
    })
}

#[cfg(windows)]
async fn assert_sync_waits_for_locked_files(
    store: &Store,
    bucket: String,
    root: &std::path::Path,
) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let folder = root.join("locked-files");
    std::fs::create_dir(&folder)?;
    let job = SyncJob {
        prefix: format!("{}/", Uuid::new_v4()),
        folder,

        id: Uuid::new_v4(),
        profile: Uuid::new_v4(),
        bucket,
        bucket_region: None,
        direction: Direction::Both,

        automatic: false,
        interval_minutes: 5,
        watch_local: false,
    };
    let snapshot = root.join("locked-files.json");
    let local = |name: &str| job.folder.join(name);
    let remote = |name: &str| format!("{}{name}", job.prefix);
    let exclusive = |path: &std::path::Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(path)
    };
    for name in ["locked.txt", "delete-local.txt", "delete-remote.txt"] {
        std::fs::write(local(name), b"initial")?;
    }
    run_with_snapshot(store.clone(), job.clone(), snapshot.clone()).await?;

    let obsolete = store.head(&remote("delete-local.txt")).await?;
    store
        .delete_if_unchanged(&remote("delete-local.txt"), obsolete.value())
        .await?;
    std::fs::remove_file(local("delete-remote.txt"))?;
    std::fs::write(local("z-normal.txt"), b"first")?;

    let before: BTreeMap<String, Pair> = read(&snapshot)?;
    let lock = exclusive(&local("locked.txt"))?;
    let slots = TransferSlots::new(4);
    let cancel = CancellationToken::new();
    let plan = build_plan(
        store,
        job.clone(),
        &snapshot,
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await?;
    assert!(
        plan.items()
            .iter()
            .any(|item| item.path() == "locked.txt" && item.action == Action::Busy)
    );
    let error = execute_plan(
        store,
        plan,
        &snapshot,
        &BTreeMap::new(),
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await
    .unwrap_err();
    assert!(error.is::<SyncDeferred>());
    drop(lock);
    let after: BTreeMap<String, Pair> = read(&snapshot)?;
    assert!(before.get("locked.txt") == after.get("locked.txt"));
    assert!(local("delete-local.txt").is_file());
    assert_eq!(
        store
            .preview_text(&remote("delete-remote.txt"), 100)
            .await?,
        "initial"
    );
    assert_eq!(
        store.preview_text(&remote("z-normal.txt"), 100).await?,
        "first"
    );

    // Introduce the next lock after planning, when the download is about to overwrite.
    let staged = root.join("replacement.txt");
    std::fs::write(&staged, b"replacement")?;
    let old = store.head(&remote("locked.txt")).await?;
    store
        .upload_file(
            &remote("locked.txt"),
            &staged,
            11,
            Arc::new(AtomicU64::new(0)),
            Some(old.value()),
        )
        .await?;
    let job = SyncJob {
        direction: Direction::Download,
        ..job.clone()
    };
    let plan = build_plan(
        store,
        job.clone(),
        &snapshot,
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await?;
    let mut lock = None;
    let mut report = |state: &SyncProgress| {
        if state.stage == "下载文件" && state.current == "locked.txt" && lock.is_none() {
            lock = Some(exclusive(&local("locked.txt")).expect("fixture should lock the target"));
        }
    };
    let error = execute_plan(
        store,
        plan,
        &snapshot,
        &BTreeMap::new(),
        &slots,
        &cancel,
        &mut report,
    )
    .await
    .unwrap_err();
    assert!(error.is::<SyncDeferred>());
    assert!(lock.is_some());
    drop(lock);
    assert_eq!(std::fs::read(local("locked.txt"))?, b"initial");
    assert!(local("delete-local.txt").is_file());
    assert!(store.head(&remote("delete-remote.txt")).await.is_ok());
    run_with_snapshot(store.clone(), job.clone(), snapshot.clone()).await?;
    assert_eq!(std::fs::read(local("locked.txt"))?, b"replacement");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable loopback S3 endpoint"]
async fn loopback_sync_continues_after_single_upload_fails() -> Result<()> {
    let (store, bucket) = loopback_store().await?;
    let root = tempfile::tempdir()?;
    let folder = root.path().join("local");
    std::fs::create_dir(&folder)?;
    let job = SyncJob {
        id: Uuid::new_v4(),
        profile: Uuid::new_v4(),
        prefix: format!("{}/", Uuid::new_v4()),
        folder,
        bucket,
        bucket_region: None,
        direction: Direction::Both,

        automatic: false,
        interval_minutes: 5,
        watch_local: false,
    };
    let first = format!("{}a-concurrent.txt", job.prefix);
    let later = format!("{}z-continue.txt", job.prefix);
    let conflict = format!("{}m-conflict.txt", job.prefix);
    std::fs::write(job.folder.join("a-concurrent.txt"), "local")?;
    std::fs::write(job.folder.join("m-conflict.txt"), "local")?;
    std::fs::write(job.folder.join("z-continue.txt"), "completed")?;
    store.create_file(conflict).await?;
    let snapshot = root.path().join("snapshot.json");
    let slots = TransferSlots::new(1);
    let cancel = CancellationToken::new();
    let plan = build_plan(
        &store,
        job.clone(),
        &snapshot,
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await?;
    store.create_file(first.clone()).await?;

    let error = execute_plan(
        &store,
        plan,
        &snapshot,
        &BTreeMap::new(),
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await
    .unwrap_err();
    assert!(error.is::<SyncDeferred>(), "{error:#}");
    let summary = error.to_string();
    assert!(
        summary.contains("a-concurrent.txt") && summary.contains("m-conflict.txt"),
        "{summary}"
    );
    let baseline: BTreeMap<String, Pair> = read(&snapshot)?;
    assert!(baseline.contains_key("z-continue.txt"));
    assert!(!baseline.contains_key("a-concurrent.txt"));
    assert!(!baseline.contains_key("m-conflict.txt"));
    assert_eq!(store.preview_text(&later, 100).await?, "completed");
    assert_eq!(store.preview_text(&first, 100).await?, "");

    let retry = error.downcast_ref::<SyncDeferred>().unwrap();
    assert_eq!(retry.paths(), ["a-concurrent.txt"]);
    let original = store.head(&first).await?;
    store.delete_if_unchanged(&first, original.value()).await?;
    std::fs::write(job.folder.join("z-continue.txt"), "changed since last run")?;
    let plan = super::plan::build_plan_with_paths(
        &store,
        job,
        &snapshot,
        Some(retry.paths()),
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await?;
    assert_eq!(
        plan.items()
            .iter()
            .map(|item| item.path())
            .collect::<Vec<_>>(),
        ["a-concurrent.txt"]
    );
    execute_plan(
        &store,
        plan,
        &snapshot,
        &BTreeMap::new(),
        &slots,
        &cancel,
        &mut ignore_progress,
    )
    .await?;
    assert_eq!(store.preview_text(&first, 100).await?, "local");
    assert_eq!(store.preview_text(&later, 100).await?, "completed");
    let after_retry: BTreeMap<String, Pair> = read(&snapshot)?;
    assert!(after_retry.contains_key("a-concurrent.txt"));
    assert!(baseline.get("z-continue.txt") == after_retry.get("z-continue.txt"));
    Ok(())
}
