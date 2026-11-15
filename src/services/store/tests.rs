use super::{
    Entry, MIB, RelocateProgress, Server, TransferCancelled, TransferSlots, child_key,
    download::decode_preview_text, etag::multipart_etag, parent_prefix, trash,
    upload::tracked_body, upload_plan,
};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::{config::Credentials, primitives::ByteStream};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn tracked_upload_body_preserves_length_and_progress_on_retry() -> Result<()> {
    let sent = Arc::new(AtomicU64::new(0));
    let body = tracked_body(ByteStream::from_static(b"test"), sent.clone()).into_inner();
    let retry = body.try_clone().context("upload body must be retryable")?;
    assert_eq!(body.content_length(), Some(4));
    assert_eq!(retry.content_length(), Some(4));
    assert_eq!(
        ByteStream::new(body).collect().await?.into_bytes().as_ref(),
        b"test"
    );
    assert_eq!(
        ByteStream::new(retry)
            .collect()
            .await?
            .into_bytes()
            .as_ref(),
        b"test"
    );
    assert_eq!(sent.load(Ordering::Relaxed), 4);
    Ok(())
}

#[tokio::test]
async fn transfer_slots_track_workers_and_resize_without_interrupting_requests() -> Result<()> {
    let slots = TransferSlots::new(2);
    let token = CancellationToken::new();
    let first = slots.acquire_worker(&token, "/first".into()).await?;
    let second = slots.acquire_worker(&token, "/second".into()).await?;
    assert_eq!(
        slots.workers(),
        (2, vec![(1, "/first".into()), (2, "/second".into())])
    );
    slots.set_limit(1);
    let queued = slots.acquire_worker(&token, "/queued".into());
    tokio::pin!(queued);
    assert!(futures_util::poll!(&mut queued).is_pending());
    drop(first);
    assert!(futures_util::poll!(&mut queued).is_pending());
    slots.set_limit(2);
    let third = queued.await?;
    assert_eq!(slots.workers().1[0], (1, "/queued".into()));

    let cancelled = token.child_token();
    let request = slots.acquire_worker(&cancelled, "/cancelled".into());
    tokio::pin!(request);
    assert!(futures_util::poll!(&mut request).is_pending());
    cancelled.cancel();
    assert!(matches!(request.await, Err(error) if error.is::<TransferCancelled>()));
    drop((second, third));
    assert!(slots.acquire_worker(&token, "/next".into()).await.is_ok());
    Ok(())
}

#[test]
fn multipart_etag_uses_exact_uneven_boundaries() -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        assert_eq!(
            multipart_etag(&b"abcdefgh"[..], &[3, 5]).await?,
            "090b68bde6dcfd1d01865596c51bad76-2"
        );
        assert!(multipart_etag(&b"abcdefgh"[..], &[3, 4]).await.is_err());
        Ok(())
    })
}

#[test]
fn recursive_plan_keeps_relative_keys_and_empty_directories() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("资料");
    std::fs::create_dir_all(root.join("a/空"))?;
    std::fs::write(root.join("a/1 #.txt"), b"abc")?;
    let keys = upload_plan("dest/", vec![root])?
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    assert_eq!(keys, ["dest/资料/a/1 #.txt", "dest/资料/a/空/"]);
    Ok(())
}

#[test]
fn key_boundaries() {
    assert_eq!(parent_prefix("one/two/"), "one/");
    assert_eq!(parent_prefix("one//"), "one/");
    assert!(child_key("one/", "..", false).is_err());
    assert!(child_key("one/", "a/b", false).is_err());
    assert!(child_key("", &"测".repeat(342), false).is_err());
}

#[test]
fn preview_decodes_utf8_and_bom_marked_utf16() -> Result<()> {
    for (bytes, expected) in [
        (Vec::new(), ""),
        ("中\r\n".as_bytes().to_vec(), "中\r\n"),
        (b"\xef\xbb\xbfhello".to_vec(), "hello"),
        (vec![0xff, 0xfe, 0x2d, 0x4e], "中"),
        (vec![0xfe, 0xff, 0x4e, 0x2d], "中"),
    ] {
        let (text, encoding) = super::decode_editable_text(bytes.clone())?;
        assert_eq!(text, expected);
        assert_eq!(encoding.encode(&text), bytes);
    }
    assert!(decode_preview_text(vec![0xff, 0xfe, 0x2d]).is_err());
    assert!(decode_preview_text(b"binary\0data".to_vec()).is_err());
    assert!(decode_preview_text(vec![0xff]).is_err());
    Ok(())
}

#[test]
#[ignore = "requires a disposable loopback S3 bucket"]
fn loopback_s3_workflow() -> Result<()> {
    let endpoint = std::env::var("S3FS_TEST_ENDPOINT")?;
    ensure!(
        endpoint.starts_with("http://127.0.0.1:") || endpoint.starts_with("https://127.0.0.1:"),
        "测试端点必须在本机"
    );
    let bucket = std::env::var("S3FS_TEST_BUCKET")?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let credentials = Credentials::new("local-fixture", "local-fixture", None, None, "test");
        let server = Server::connect(
            "us-east-1".into(),
            endpoint,
            String::new(),
            Some(credentials),
        )
        .await?;
        let store = server
            .buckets()
            .await?
            .into_iter()
            .find(|item| item.name() == bucket)
            .context("服务器未列出测试桶")?
            .open(&server);
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("资料");
        std::fs::create_dir_all(input.join("嵌套/空"))?;
        std::fs::write(input.join("嵌套/1 # %.txt"), b"payload")?;
        let large = std::fs::File::create(input.join("large.bin"))?;
        large.set_len(65 * MIB)?;
        drop(large);

        assert_eq!(
            store
                .upload(String::new(), vec![input], |_, _, _, _| {})
                .await?,
            3
        );
        let source = store
            .list("", true)
            .await?
            .into_iter()
            .find(|row| row.key() == "资料/")
            .context("源目录不存在")?;
        assert!(source.directory());
        let mut progress = Vec::new();
        store
            .relocate_to_with_progress(
                &store,
                source,
                "归档/".into(),
                &CancellationToken::new(),
                |phase| {
                    progress.push(phase);
                },
            )
            .await?;
        let expected = (0..=3)
            .map(|done| RelocateProgress::Copying { done, total: 3 })
            .chain((0..=3).map(|done| RelocateProgress::Removing { done, total: 3 }))
            .collect::<Vec<_>>();
        assert_eq!(progress, expected);
        assert!(store.list("资料/", false).await?.is_empty());
        let rows = store.list("归档/", false).await?;
        assert!(rows.iter().any(|row| row.key() == "归档/嵌套/空/"));
        let large = rows
            .iter()
            .find(|row| row.key() == "归档/large.bin")
            .context("递归复制后找不到大文件")?;
        assert_eq!(large.size(), 65 * MIB as i64);
        let preview = temp.path().join("large-preview");
        let received = std::sync::atomic::AtomicU64::new(0);
        let workers = TransferSlots::new(3);
        store
            .download_preview_with_workers(large, &preview, 65 * MIB, &received, &workers)
            .await?;
        assert_eq!(std::fs::metadata(&preview)?.len(), 65 * MIB);
        assert_eq!(
            received.load(std::sync::atomic::Ordering::Relaxed),
            65 * MIB
        );
        assert!(workers.workers().1.is_empty());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let aborted = temp.path().join("cancelled-preview");
        let error = store
            .download_with_progress(large, &aborted, &received, &workers, &cancelled)
            .await
            .unwrap_err();
        assert!(error.is::<TransferCancelled>());
        assert!(!aborted.exists());
        let file = rows
            .into_iter()
            .find(|row| row.key() == "归档/嵌套/1 # %.txt")
            .context("递归复制后找不到文件")?;
        store
            .relocate(file, "归档/嵌套/已重命名.txt".into())
            .await?;
        let file = store
            .list("归档/嵌套/", true)
            .await?
            .into_iter()
            .find(|row| row.key() == "归档/嵌套/已重命名.txt")
            .context("重命名后找不到文件")?;
        let empty = Entry::folder("归档/嵌套/空/".into());
        store.trash(empty.clone()).await?;
        assert!(store.list(empty.key(), false).await?.is_empty());
        let trashed = store
            .list_trash()
            .await?
            .into_iter()
            .find(|record| record.original() == empty.key())
            .context("回收站中找不到空目录")?;
        assert!(
            store
                .list_objects(".rs3fs-trash/entries/", false)
                .await?
                .iter()
                .any(|entry| entry.key() == format!(".rs3fs-trash/entries/{}.json", trashed.id()))
        );
        assert!(store.list_objects(".s3fs-trash/", false).await?.is_empty());
        store.restore_trash(&trashed).await?;
        assert_eq!(store.head(empty.key()).await?.size, Some(0));
        store.delete_permanently(empty.clone()).await?;
        assert!(store.list(empty.key(), false).await?.is_empty());

        for (original, restore) in [("legacy-restore.txt", true), ("legacy-purge.txt", false)] {
            let id = uuid::Uuid::new_v4();
            let manifest = format!(".s3fs-trash/entries/{id}.json");
            let copies = format!(".s3fs-trash/objects/{id}/");
            let record = serde_json::json!({
                "id": id,
                "original": original,
                "directory": false,
                "deleted_at": 1,
                "objects": [{ "key": original, "etag": "" }],
                "pending": false,
                "interrupted": false,
            });
            store
                .put(&manifest, ByteStream::from(serde_json::to_vec(&record)?))
                .await?;
            store
                .put(&format!("{copies}0"), ByteStream::from_static(b"legacy"))
                .await?;
            assert!(store.list(".s3fs-trash/", false).await.is_err());
            let record = store
                .list_trash()
                .await?
                .into_iter()
                .find(|record| record.id() == id)
                .context("回收站中找不到旧版记录")?;
            if restore {
                store.restore_trash(&record).await?;
                assert_eq!(store.preview_text(original, 6).await?, "legacy");
            } else {
                store.purge_trash(&record).await?;
                assert!(store.head(original).await.is_err());
            }
            assert!(store.list_objects(&copies, false).await?.is_empty());
            assert!(store.list_objects(&manifest, false).await?.is_empty());
        }

        let output = temp.path().join("download.txt");
        store
            .download(file.key().to_owned(), output.clone())
            .await?;
        assert_eq!(std::fs::read(&output)?, b"payload");
        let tag = store.head(file.key()).await?;
        assert!(
            store
                .compare_etag_progress(file.key(), &tag, &output, |_| {})
                .await?
                .starts_with("本地 MD5 与 ETag 一致")
        );
        assert_eq!(store.preview_text(file.key(), 7).await?, "payload");
        assert!(store.preview_text(file.key(), 6).await.is_err());
        assert!(store.create_file(file.key().to_owned()).await.is_err());
        assert_eq!(store.preview_text(file.key(), 7).await?, "payload");
        let new_file = "归档/嵌套/新建.txt";
        store.create_file(new_file.to_owned()).await?;
        assert_eq!(store.preview_text(new_file, 7).await?, "");
        let original = store.head(new_file).await?;
        let etag = original.value();
        let updated = store
            .save_text(new_file, etag, b"updated text".to_vec())
            .await?
            .context("保存响应缺少 ETag")?;
        assert_ne!(updated, etag);
        assert!(
            store
                .save_text(new_file, etag, b"stale write".to_vec())
                .await
                .is_err()
        );
        assert_eq!(
            store
                .preview_text_with_progress(new_file, Some(&updated), MIB, &received)
                .await?,
            "updated text"
        );
        let spaced = "编码测试/a b.txt";
        let plus = "编码测试/a+b.txt";
        store.put(spaced, ByteStream::from_static(b"space")).await?;
        store.put(plus, ByteStream::from_static(b"plus")).await?;
        let encoded_rows = store.list("编码测试/", true).await?;
        assert_eq!(encoded_rows.len(), 2);
        for (key, expected, destination) in [
            (spaced, "space", "编码测试/空 格.txt"),
            (plus, "plus", "编码测试/加+号.txt"),
        ] {
            let row = encoded_rows
                .iter()
                .find(|row| row.key() == key)
                .context("列表未保留原始对象键")?;
            store.copy(row.clone(), destination.to_owned()).await?;
            assert_eq!(store.preview_text(destination, 8).await?, expected);
            assert_eq!(store.preview_text(key, 8).await?, expected);
        }
        store.trash(Entry::folder("归档/".into())).await?;
        assert!(store.list("归档/", false).await?.is_empty());
        assert!(
            store
                .list("", true)
                .await?
                .iter()
                .all(|row| !trash::reserved(row.key()))
        );
        let entries = store.list_trash().await?;
        let record = entries
            .iter()
            .find(|record| record.original() == "归档/")
            .context("回收站缺少已删除目录")?;
        let path = "归档/嵌套/已重命名.txt";
        store
            .put(path, ByteStream::from_static(b"conflict"))
            .await?;
        assert!(store.restore_trash(record).await.is_err());
        let mut rows = store.list("归档/", false).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size(), 8);
        store.delete_object(&rows.remove(0)).await?;
        store.restore_trash(record).await?;
        assert_eq!(store.list("归档/", false).await?.len(), 3);
        assert!(
            store
                .list_trash()
                .await?
                .iter()
                .all(|entry| entry.id() != record.id())
        );

        store.trash(Entry::folder("归档/".into())).await?;
        let entries = store.list_trash().await?;
        let record = entries
            .iter()
            .find(|record| record.original() == "归档/")
            .context("回收站缺少待清理目录")?;
        store.purge_trash(record).await?;
        assert!(
            store
                .list_trash()
                .await?
                .iter()
                .all(|entry| entry.id() != record.id())
        );
        let path = "changed-before-trash.txt";
        store.put(path, ByteStream::from_static(b"old")).await?;
        let stale = store.list(path, false).await?.remove(0);
        store.delete_object(&stale).await?;
        store
            .put(path, ByteStream::from_static(b"replacement"))
            .await?;
        assert!(store.trash(stale).await.is_err());
        let mut current = store.list(path, false).await?;
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].size(), 11);
        let entries = store.list_trash().await?;
        let record = entries
            .iter()
            .find(|record| record.original() == path)
            .context("回收站缺少未完成记录")?;
        assert!(record.interrupted());
        store.purge_trash(record).await?;
        store.delete_object(&current.remove(0)).await?;
        Ok(())
    })
}

#[test]
#[ignore = "requires a disposable loopback S3 bucket"]
fn journal_resume_loopback() -> Result<()> {
    use crate::services::transfer_journal::{
        JournalState, TransferJournal, TransferOperation, TransferRecord,
    };
    use sha2::{Digest, Sha256};

    let endpoint = std::env::var("S3FS_TEST_ENDPOINT")?;
    ensure!(
        endpoint.starts_with("http://127.0.0.1:"),
        "测试端点必须在本机"
    );
    let bucket = std::env::var("S3FS_TEST_BUCKET")?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let credentials = Credentials::new("local-fixture", "local-fixture", None, None, "test");
        let server = Server::connect(
            "us-east-1".into(),
            endpoint,
            String::new(),
            Some(credentials),
        )
        .await?;
        let store = super::Bucket::named(&bucket).open(&server);
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("journal");
        let slots = TransferSlots::new(4);
        let cancel = CancellationToken::new();

        store.create_file("resume/from/a".into()).await?;
        store.create_file("resume/from/b".into()).await?;
        let remote = TransferRecord::new(
            "fixture".into(),
            bucket.clone(),
            TransferOperation::Remote {
                target_bucket: bucket.clone(),
                targets: vec![(Entry::folder("resume/from/".into()), "resume/to/".into())],
                copying: false,
                rename: true,
            },
        );
        let journal = TransferJournal::open_at(&root, remote.clone())?;
        assert!(TransferJournal::open_at(&root, remote.clone()).is_err());
        let worker_store = store.clone();
        let interrupted = tokio::spawn(async move {
            journal
                .run(
                    worker_store.clone(),
                    worker_store,
                    TransferSlots::new(1),
                    CancellationToken::new(),
                    Arc::new(AtomicU64::new(0)),
                    Default::default(),
                    |message| {
                        if message == "复制对象 1/2" {
                            panic!("simulated interruption after checkpoint");
                        }
                    },
                )
                .await
        })
        .await;
        assert!(interrupted.is_err());
        assert_eq!(store.list("resume/to/", false).await?.len(), 1);
        assert_eq!(store.list("resume/from/", false).await?.len(), 2);
        let journal = TransferJournal::open_at(&root, remote.clone())?;
        assert!(matches!(journal.record().state, JournalState::Pending));
        journal
            .run(
                store.clone(),
                store.clone(),
                slots.clone(),
                cancel.clone(),
                Arc::new(AtomicU64::new(0)),
                Default::default(),
                |_| {},
            )
            .await?;
        assert!(store.list("resume/from/", false).await?.is_empty());
        assert_eq!(store.list("resume/to/", false).await?.len(), 2);
        assert!(matches!(
            TransferJournal::open_at(&root, remote)?.record().state,
            JournalState::Complete
        ));

        let local = temp.path().join("multipart-resume.bin");
        let first_part = vec![0x5a; (16 * MIB) as usize];
        let mut handle = std::fs::File::create(&local)?;
        use std::io::Write;
        handle.write_all(&first_part)?;
        handle.set_len(65 * MIB)?;
        drop(handle);
        let key = "resume/multipart-resume.bin";
        let upload = TransferRecord::new(
            "fixture".into(),
            bucket.clone(),
            TransferOperation::Upload {
                prefix: "resume/".into(),
                paths: vec![local.clone()],
            },
        );
        let journal = TransferJournal::open_at(&root, upload)?;
        let session = store
            .client
            .create_multipart_upload()
            .bucket(&bucket)
            .key(key)
            .send()
            .await?;
        let session = session.upload_id().context("测试服务没有返回上传 ID")?;
        let uploaded_part = store
            .client
            .upload_part()
            .bucket(&bucket)
            .key(key)
            .upload_id(session)
            .part_number(1)
            .body(ByteStream::from(first_part.clone()))
            .send()
            .await?;
        journal.save(
            "multipart-0.json",
            &serde_json::json!({
                "id": session,
                "parts": { "0": uploaded_part.e_tag().context("测试分片缺少 ETag")? }
            }),
        )?;
        journal.save("upload-start-0.json", &true)?;
        journal
            .run(
                store.clone(),
                store.clone(),
                slots.clone(),
                cancel.clone(),
                Arc::new(AtomicU64::new(0)),
                Default::default(),
                |_| {},
            )
            .await?;
        let object = store
            .list("resume/", true)
            .await?
            .into_iter()
            .find(|entry| entry.key() == key)
            .context("找不到续传完成的对象")?;
        assert_eq!(object.size(), (65 * MIB) as i64);

        let destination = temp.path().join("saved.bin");
        let download = TransferRecord::new(
            "fixture".into(),
            bucket,
            TransferOperation::Download {
                entry: object,
                destination: destination.clone(),
            },
        );
        let journal = TransferJournal::open_at(&root, download)?;
        let part = journal.part_path(&destination, 0)?;
        std::fs::write(&part, &first_part)?;
        journal.save("download-0.json", &serde_json::json!({"original": null}))?;
        journal.save(
            "range-0-0.json",
            &super::hex_digest(&Sha256::digest(&first_part)),
        )?;
        let counter = Arc::new(AtomicU64::new(0));
        let mut initial = None;
        journal
            .run(
                store.clone(),
                store.clone(),
                slots.clone(),
                cancel.clone(),
                counter,
                Default::default(),
                |progress| {
                    if initial.is_none() {
                        initial = Some(progress);
                    }
                },
            )
            .await?;
        assert_eq!(initial.as_deref(), Some("下载字节 16777216/68157440"));
        assert_eq!(std::fs::read(destination)?, std::fs::read(local)?);
        assert!(!part.exists());
        #[cfg(windows)]
        {
            let clipboard = TransferRecord::new(
                "fixture".into(),
                store.bucket().to_owned(),
                TransferOperation::Clipboard {
                    entries: vec![Entry::folder("resume/to/".into())],
                },
            );
            let journal = TransferJournal::open_at(&root, clipboard.clone())?;
            journal
                .run(
                    store.clone(),
                    store,
                    slots,
                    cancel,
                    Arc::new(AtomicU64::new(0)),
                    Default::default(),
                    |_| {},
                )
                .await?;
            let journal = TransferJournal::open_at(&root, clipboard)?;
            let paths: Vec<std::path::PathBuf> = journal
                .load("clipboard-paths.json")?
                .context("缺少剪贴板缓存")?;
            assert_eq!(paths.len(), 1);
            assert!(paths[0].join("a").is_file());
            assert!(paths[0].join("b").is_file());
        }
        Ok(())
    })
}
