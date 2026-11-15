use super::{local::LocalFile, *};

#[test]
#[ignore = "requires a disposable loopback S3 endpoint"]
fn loopback_sync_and_capacity() -> Result<()> {
    let endpoint = std::env::var("S3FS_TEST_ENDPOINT")?;
    let bucket = std::env::var("S3FS_TEST_BUCKET")?;
    ensure!(
        endpoint.starts_with("http://127.0.0.1:"),
        "测试端点必须在本机"
    );
    tokio::runtime::Runtime::new()?.block_on(async {
        let credentials = aws_sdk_s3::config::Credentials::new(
            "local-fixture",
            "local-fixture",
            None,
            None,
            "test",
        );
        let server = crate::services::store::Server::connect(
            "us-east-1".into(),
            endpoint,
            String::new(),
            Some(credentials),
        )
        .await?;
        let store = server.open(bucket.clone(), None);
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
            conflict_policy: ConflictPolicy::default(),
            automatic: false,
            propagate_deletes: false,
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
                .await?
                .contains("上传 1")
        );
        assert_eq!(store.preview_text(&key, 100).await?, "two!");

        let remote = temp.path().join("remote.txt");
        std::fs::write(&remote, b"remote")?;
        let tag = store.head(&key).await?;
        store
            .upload_file(
                &key,
                &remote,
                6,
                Arc::new(AtomicU64::new(0)),
                Some(tag.value()),
            )
            .await?;
        assert!(
            run_with_snapshot(store.clone(), job.clone(), snapshot.clone())
                .await?
                .contains("下载 1")
        );
        assert_eq!(std::fs::read(&local)?, b"remote");

        std::fs::write(&local, b"local")?;
        let tag = store.head(&key).await?;
        std::fs::write(&remote, b"other")?;
        store
            .upload_file(
                &key,
                &remote,
                5,
                Arc::new(AtomicU64::new(0)),
                Some(tag.value()),
            )
            .await?;
        assert!(
            run_with_snapshot(store.clone(), job.clone(), snapshot)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&local)?, b"local");
        assert_eq!(store.preview_text(&key, 100).await?, "other");
        let delete_job = SyncJob {
            propagate_deletes: true,
            ..job.clone()
        };
        let delete_state = temp.path().join("deletions.json");
        let local_delete = delete_job.folder.join("local-delete.txt");
        let remote_delete = delete_job.folder.join("remote-delete.txt");
        std::fs::write(&local_delete, b"local-delete")?;
        std::fs::write(&remote_delete, b"remote-delete")?;
        // The older conflicting file is intentionally left untouched.
        let _ = run_with_snapshot(store.clone(), delete_job.clone(), delete_state.clone()).await;
        let local_key = format!("{}local-delete.txt", delete_job.prefix);
        let remote_key = format!("{}remote-delete.txt", delete_job.prefix);
        assert_eq!(store.preview_text(&local_key, 100).await?, "local-delete");
        let tag = store.head(&remote_key).await?;
        store.delete_if_unchanged(&remote_key, tag.value()).await?;
        std::fs::remove_file(&local_delete)?;
        let result =
            run_with_snapshot(store.clone(), delete_job.clone(), delete_state.clone()).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("冲突 1"));
        let state: BTreeMap<String, Pair> = read(&delete_state)?;
        assert!(!state.contains_key("remote-delete.txt"));
        assert!(!state.contains_key("local-delete.txt"));
        assert!(!remote_delete.exists());
        assert!(store.list(&local_key, false).await?.is_empty());
        assert_eq!(std::fs::read(&local)?, b"local");
        // Recreating the same bytes must start a new sync, not repeat an old deletion.
        std::fs::write(&local_delete, b"local-delete")?;
        let result = run_with_snapshot(store.clone(), delete_job, delete_state).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("冲突 1"));
        assert_eq!(store.preview_text(&local_key, 100).await?, "local-delete");
        std::fs::write(&local, b"other")?;
        let merged_job = SyncJob {
            direction: Direction::Both,
            ..job.clone()
        };
        let merged_state = temp.path().join("merged.json");
        let summary =
            run_with_snapshot(store.clone(), merged_job.clone(), merged_state.clone()).await?;
        assert!(summary.contains("已核对相同文件 2"));
        std::fs::write(&local, b"after merge")?;
        let summary = run_with_snapshot(store.clone(), merged_job, merged_state).await?;
        assert!(summary.contains("上传 1"));
        assert_eq!(store.preview_text(&key, 100).await?, "after merge");
        let url = url::Url::parse(&store.share_url(key, 15).await?)?;
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "X-Amz-Expires" && value == "900")
        );
        Ok(())
    })
}

#[test]
fn sync_scan_skips_office_lock_files_in_nested_folders() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::create_dir(temp.path().join("docs"))?;
    std::fs::write(temp.path().join("docs/~$report.docx"), b"lock")?;
    std::fs::write(temp.path().join("docs/report.docx"), b"document")?;
    let tree = scan_local(temp.path())?;
    assert_eq!(
        tree.files.keys().map(String::as_str).collect::<Vec<_>>(),
        ["docs/report.docx"]
    );
    Ok(())
}

#[test]
fn manual_conflict_resolution_restores_only_from_the_selected_side() {
    use Action::{Conflict, Download, Upload};
    use ConflictPolicy::{PreferLocal, PreferRemote};
    assert_eq!(PreferLocal.resolve_once(Conflict, true, false), Upload);
    assert_eq!(PreferRemote.resolve_once(Conflict, true, false), Conflict);
    assert_eq!(PreferRemote.resolve_once(Conflict, false, true), Download);
    assert_eq!(PreferLocal.resolve_once(Conflict, false, true), Conflict);
}

#[test]
fn conflict_priority_only_resolves_two_existing_files_in_bidirectional_jobs() {
    use Action::{Conflict, DeleteRemote, Download, Upload};
    use ConflictPolicy::{PreferLocal, PreferRemote, PreserveBoth};
    assert_eq!(
        PreserveBoth.resolve(Direction::Both, Conflict, true),
        Conflict
    );
    assert_eq!(PreferLocal.resolve(Direction::Both, Conflict, true), Upload);
    assert_eq!(
        PreferRemote.resolve(Direction::Both, Conflict, true),
        Download
    );
    for policy in ConflictPolicy::ALL {
        assert_eq!(policy.resolve(Direction::Both, Conflict, false), Conflict);
        assert_eq!(
            policy.resolve(Direction::Download, Conflict, true),
            Conflict
        );
        assert_eq!(policy.resolve(Direction::Upload, Conflict, true), Conflict);
        assert_eq!(policy.resolve(Direction::Both, Download, true), Download);
        assert_eq!(
            policy.resolve(Direction::Both, DeleteRemote, false),
            DeleteRemote
        );
    }
}

#[test]
fn sync_conflicts_preserve_both_sides() {
    let local = LocalFile {
        bytes: 1,
        digest: [0; 16],
    };
    let changed = LocalFile {
        bytes: 1,
        digest: [1; 16],
    };
    let remote = RemoteFile {
        bytes: 1,
        etag: "first".into(),
    };
    let remote_changed = RemoteFile {
        bytes: 1,
        etag: "second".into(),
    };
    let pair = Pair {
        local: local.clone(),
        remote: remote.clone(),
    };
    assert_eq!(
        decide(Direction::Both, true, Some(&pair), Some(&changed), None),
        Action::Conflict
    );
    assert_eq!(
        decide(
            Direction::Both,
            true,
            Some(&pair),
            None,
            Some(&remote_changed)
        ),
        Action::Conflict
    );
    assert_eq!(
        decide(Direction::Upload, true, Some(&pair), Some(&local), None),
        Action::Conflict
    );
    assert_eq!(
        decide(Direction::Download, true, Some(&pair), None, Some(&remote)),
        Action::Conflict
    );
    assert_eq!(
        decide(
            Direction::Both,
            false,
            Some(&pair),
            Some(&changed),
            Some(&remote)
        ),
        Action::Upload
    );
    assert_eq!(
        decide(
            Direction::Both,
            false,
            Some(&pair),
            Some(&local),
            Some(&remote_changed)
        ),
        Action::Download
    );
    assert_eq!(
        decide(
            Direction::Both,
            false,
            Some(&pair),
            Some(&changed),
            Some(&remote_changed)
        ),
        Action::Conflict
    );
    assert_eq!(
        decide(Direction::Both, false, None, Some(&local), Some(&remote)),
        Action::Conflict
    );
    assert_eq!(
        decide(Direction::Both, false, Some(&pair), None, Some(&remote)),
        Action::Conflict
    );
    for path in ["..", "C:", "file:stream", "CON.txt", "COM¹.txt", "a."] {
        assert!(!valid_component(path), "{path}");
    }
}
