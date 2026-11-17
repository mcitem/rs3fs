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

#[test]
fn rename_without_overwrite_preserves_both_files_on_conflict() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("partial");
    let destination = temp.path().join("result");
    std::fs::write(&source, b"download")?;
    std::fs::write(&destination, b"local")?;

    let error = super::rename_noclobber(&source, &destination).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&source)?, b"download");
    assert_eq!(std::fs::read(&destination)?, b"local");

    std::fs::remove_file(&destination)?;
    super::rename_noclobber(&source, &destination)?;
    assert!(!source.exists());
    assert_eq!(std::fs::read(&destination)?, b"download");
    Ok(())
}

#[tokio::test]
async fn resumed_download_preserves_late_local_changes() -> Result<()> {
    use crate::services::transfer_journal::{TransferJournal, TransferOperation, TransferRecord};
    use sha2::{Digest, Sha256};

    let store = Server::connect(
        "us-east-1".into(),
        "http://127.0.0.1:1".into(),
        String::new(),
        Some(Credentials::new("local", "local", None, None, "test")),
    )
    .await?
    .open("source".into(), None);
    let slots = TransferSlots::new(1);
    let cancel = CancellationToken::new();
    for replace in [false, true] {
        let temp = tempfile::tempdir()?;
        let destination = temp.path().join("result");
        if replace {
            std::fs::write(&destination, b"original")?;
        }
        let overwrite = super::file_stamp(&destination)?;
        let entry = Entry::file("object".into(), 4, "\"etag\"".into());
        let journal = TransferJournal::open_at(
            temp.path(),
            TransferRecord::new(
                "fixture".into(),
                "source".into(),
                TransferOperation::Download {
                    entry: entry.clone(),
                    destination: destination.clone(),
                    overwrite: overwrite.clone(),
                },
            ),
        )?;
        let part = journal.part_path(&destination, 0)?;
        std::fs::write(&part, b"data")?;
        journal.save("download-0.json", &serde_json::json!({"original": overwrite}))?;
        journal.save("range-0-0.json", &super::hex_digest(&Sha256::digest(b"data")))?;
        let error = store
            .journal_download(
                &journal,
                0,
                &entry,
                &destination,
                overwrite.as_ref(),
                &slots,
                &cancel,
                &AtomicU64::new(0),
                &mut |progress| {
                    if progress == "下载字节 4/4" {
                        std::fs::write(&destination, b"local edit").unwrap();
                    }
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("本地目标文件已变化"));
        assert_eq!(std::fs::read(&destination)?, b"local edit");
        assert_eq!(std::fs::read(&part)?, b"data");
    }
    Ok(())
}

#[tokio::test]
async fn sync_content_check_skips_body_only_when_head_confirms_md5() -> Result<()> {
    use md5::{Digest, Md5};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let digest: [u8; 16] = Md5::digest(b"data").into();
    let md5_tag = format!("\"{}\"", super::hex_digest(&digest));
    for (etag, expected, method, body) in [
        (md5_tag.as_str(), true, "HEAD", ""),
        ("\"opaque\"", true, "GET", "data"),
        ("\"opaque\"", false, "GET", "diff"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let fixture = async {
            let (socket, _) = listener.accept().await?;
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await?;
            let mut request = line.split_whitespace();
            assert_eq!(request.next(), Some(method));
            let target = request.next().context("缺少请求路径")?;
            assert_eq!(target.split('?').next(), Some("/source/file"));
            let mut has_if_match = false;
            loop {
                line.clear();
                if socket.read_line(&mut line).await? == 0 || line == "\r\n" {
                    break;
                }
                has_if_match |= line.eq_ignore_ascii_case(&format!("if-match: {etag}\r\n"));
            }
            if method == "GET" {
                assert!(has_if_match);
            }
            socket
                .get_mut()
                .write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 4\r\nETag: {etag}\r\n\r\n{body}").as_bytes())
                .await?;
            Ok::<_, anyhow::Error>(())
        };
        let compare = async {
            let store = Server::connect(
                "us-east-1".into(),
                endpoint,
                String::new(),
                Some(Credentials::new("local", "local", None, None, "test")),
            )
            .await?
            .open("source".into(), None);
            let entry = Entry::file("file".into(), 4, etag.to_owned());
            let matches = store
                .matches_contents(
                    &entry,
                    &digest,
                    &TransferSlots::new(1),
                    &CancellationToken::new(),
                )
                .await?;
            assert_eq!(matches, expected);
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::try_join!(fixture, compare)
        })
        .await??;
    }
    Ok(())
}

#[tokio::test]
async fn saving_text_carries_tags_even_without_a_tag_count_header() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let fixture = async {
        for (request, headers, body) in [
            (
                "HEAD /source/file ",
                "ETag: \"old\"\r\nx-amz-storage-class: STANDARD_IA\r\n",
                "",
            ),
            (
                "GET /source/file?tagging",
                "Content-Type: application/xml\r\n",
                "<Tagging><TagSet><Tag><Key>review</Key><Value>keep</Value></Tag></TagSet></Tagging>",
            ),
            ("PUT /source/file", "ETag: \"new\"\r\n", ""),
        ] {
            let (socket, _) = listener.accept().await?;
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await?;
            assert!(line.starts_with(request), "{line}");
            let mut received_headers = String::new();
            loop {
                line.clear();
                if socket.read_line(&mut line).await? == 0 || line == "\r\n" {
                    break;
                }
                received_headers.push_str(&line.to_ascii_lowercase());
            }
            if request.starts_with("PUT ") {
                assert!(received_headers.contains("if-match: \"old\"\r\n"));
                assert!(received_headers.contains("x-amz-tagging: review=keep\r\n"));
                assert!(received_headers.contains("x-amz-storage-class: standard_ia\r\n"));
                let mut bytes = [0; 4];
                socket.read_exact(&mut bytes).await?;
                assert_eq!(&bytes, b"edit");
            }
            socket.get_mut().write_all(
                format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len()).as_bytes(),
            ).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let save = async {
        let store = Server::connect(
            "us-east-1".into(),
            endpoint,
            String::new(),
            Some(Credentials::new("local", "local", None, None, "test")),
        )
        .await?
        .open("source".into(), None);
        let tag = store.save_text("file", "\"old\"", b"edit".to_vec()).await?;
        assert_eq!(tag.as_deref(), Some("\"new\""));
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::try_join!(fixture, save)
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn move_tries_server_copy_before_local_relay_and_verifies_before_delete() -> Result<()> {
    use super::COPY_LIMIT;
    use crate::services::transfer_journal::{TransferJournal, TransferOperation, TransferRecord};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    for (fail_copy, cancel_copy, local_relay, target_changed) in [
        (false, false, false, false),
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, true, true),
    ] {
        let size = if local_relay { 4 } else { COPY_LIMIT + 1 };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let cancel = CancellationToken::new();
        let fixture = async {
            let mut requests = vec![
                ("HEAD /destination/copy", "404 Not Found", String::new(), ""),
                (
                    "HEAD /source/original",
                    "200 OK",
                    format!(
                        "Content-Length: {size}\r\nETag: \"source\"\r\nContent-Type: application/octet-stream\r\nx-amz-meta-example: value\r\nx-amz-storage-class: STANDARD_IA\r\n"
                    ),
                    "",
                ),
                (
                    "POST /destination/copy?uploads",
                    "200 OK",
                    String::new(),
                    "<InitiateMultipartUploadResult><UploadId>upload</UploadId></InitiateMultipartUploadResult>",
                ),
            ];
            let part_size = (128 * MIB) as i64;
            let count = if fail_copy || cancel_copy { 1 } else { 41 };
            for _ in 0..count {
                requests.push(if fail_copy {
                    (
                        "PUT /destination/copy?",
                        "403 Forbidden",
                        String::new(),
                        "<Error><Code>AccessDenied</Code></Error>",
                    )
                } else {
                    (
                        "PUT /destination/copy?",
                        "200 OK",
                        String::new(),
                        "<CopyPartResult><ETag>\"part\"</ETag></CopyPartResult>",
                    )
                });
            }
            if fail_copy || cancel_copy {
                requests.push((
                    "DELETE /destination/copy?",
                    "204 No Content",
                    String::new(),
                    "",
                ));
            } else {
                requests.extend([
                    ("POST /destination/copy?", "200 OK", String::new(), "<CompleteMultipartUploadResult><ETag>\"target\"</ETag></CompleteMultipartUploadResult>"),
                    ("HEAD /destination/copy", "200 OK", format!("Content-Length: {size}\r\nETag: \"target\"\r\n"), ""),
                    ("HEAD /destination/copy", "200 OK", format!("Content-Length: {size}\r\nETag: \"target\"\r\n"), ""),
                    ("DELETE /source/original", "204 No Content", String::new(), ""),
                ]);
            }

            if local_relay {
                requests = vec![
                    (
                        "PUT /destination/copy",
                        "501 Not Implemented",
                        String::new(),
                        "<Error><Code>NotImplemented</Code></Error>",
                    ),
                    ("HEAD /destination/copy", "404 Not Found", String::new(), ""),
                    (
                        "HEAD /source/original",
                        "200 OK",
                        "Content-Length: 4\r\nETag: \"source\"\r\nx-amz-meta-example: value\r\nx-amz-storage-class: STANDARD_IA\r\n"
                            .into(),
                        "",
                    ),
                    (
                        "GET /source/original",
                        "200 OK",
                        "Content-Length: 4\r\nETag: \"source\"\r\n".into(),
                        "data",
                    ),
                    (
                        "PUT /destination/copy",
                        "200 OK",
                        "ETag: \"target\"\r\n".into(),
                        "",
                    ),
                    (
                        "HEAD /destination/copy",
                        "200 OK",
                        "Content-Length: 4\r\nETag: \"target\"\r\n".into(),
                        "",
                    ),
                ];
                let observed = if target_changed { "updated" } else { "target" };
                requests.push((
                    "HEAD /destination/copy",
                    "200 OK",
                    format!("Content-Length: 4\r\nETag: \"{observed}\"\r\n"),
                    "",
                ));
                if !target_changed {
                    requests.push(("DELETE /source/original", "204 No Content", String::new(), ""));
                }
            }
            for (index, (expected, status, mut headers, response)) in
                requests.into_iter().enumerate()
            {
                let (socket, _) = listener.accept().await?;
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                socket.read_line(&mut request).await?;
                assert!(request.starts_with(expected), "{request}");
                let mut sent_headers = String::new();
                loop {
                    let mut line = String::new();
                    if socket.read_line(&mut line).await? == 0 || line == "\r\n" {
                        break;
                    }
                    sent_headers.push_str(&line.to_ascii_lowercase());
                }
                let length = sent_headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: ")?.parse::<usize>().ok())
                    .unwrap_or(0);
                assert!(length < 8192, "unexpected object body");
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await?;
                if local_relay && index == 0 {
                    assert!(sent_headers.contains("x-amz-copy-source: source/original\r\n"));
                    assert!(body.is_empty());
                } else if local_relay && index == 4 {
                    assert!(sent_headers.contains("if-none-match: *\r\n"));
                    assert!(sent_headers.contains("x-amz-meta-example: value\r\n"));
                    assert!(sent_headers.contains("x-amz-storage-class: standard_ia\r\n"));
                    assert_eq!(body, b"data");
                } else if request.starts_with("GET ") {
                    assert!(sent_headers.contains("if-match: \"source\"\r\n"));
                } else if request.starts_with("PUT ") {
                    let part = (index - 3) as i64;
                    let first = part * part_size;
                    let last = (first + part_size).min(size) - 1;
                    assert!(request.contains(&format!("partNumber={}", part + 1)));
                    assert!(request.contains("uploadId=upload"));
                    assert!(sent_headers.contains("x-amz-copy-source: source/original\r\n"));
                    assert!(sent_headers.contains("x-amz-copy-source-if-match: \"source\"\r\n"));
                    assert!(sent_headers.contains(&format!(
                        "x-amz-copy-source-range: bytes={first}-{last}\r\n"
                    )));
                    assert!(body.is_empty());
                    if cancel_copy {
                        cancel.cancel();
                    }
                } else if request.starts_with("POST /destination/copy?uploads") {
                    assert!(sent_headers.contains("x-amz-meta-example: value\r\n"));
                    assert!(sent_headers.contains("content-type: application/octet-stream\r\n"));
                    assert!(sent_headers.contains("x-amz-storage-class: standard_ia\r\n"));
                } else if request.starts_with("POST /destination/copy?") {
                    assert!(request.contains("uploadId=upload"));
                    assert!(sent_headers.contains("if-none-match: *\r\n"));
                    let body = std::str::from_utf8(&body)?;
                    assert_eq!(body.matches("<PartNumber>").count(), 41);
                    assert!(body.contains("<PartNumber>41</PartNumber>"));
                } else if request.starts_with("DELETE /source/") {
                    assert!(sent_headers.contains("if-match: \"source\"\r\n"));
                } else if request.starts_with("DELETE /destination/") {
                    assert!(request.contains("uploadId=upload"));
                }
                if !headers.starts_with("Content-Length:") {
                    headers.push_str(&format!("Content-Length: {}\r\n", response.len()));
                }
                if !response.is_empty() {
                    headers.push_str("Content-Type: application/xml\r\n");
                }
                socket
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n{response}"
                        )
                        .as_bytes(),
                    )
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let transfer = async {
            let slots = TransferSlots::new(1);
            let _upload = if local_relay {
                None
            } else {
                Some(
                    slots
                        .acquire_worker(&cancel, "unrelated upload".into())
                        .await?,
                )
            };
            let server = Server::connect(
                "us-east-1".into(),
                endpoint,
                String::new(),
                Some(Credentials::new("local", "local", None, None, "test")),
            )
            .await?
            .with_transfer_slots(slots.clone());
            let source = server.open("source".into(), None);
            let destination = server.open("destination".into(), None);
            let targets = vec![(
                Entry::file("original".into(), size, "\"source\"".into()),
                "copy".into(),
            )];
            let temp = tempfile::tempdir()?;
            let journal = TransferJournal::open_at(
                temp.path(),
                TransferRecord::new(
                    "fixture".into(),
                    "source".into(),
                    TransferOperation::Remote {
                        target_bucket: "destination".into(),
                        targets: targets.clone(),
                        copying: false,
                        rename: false,
                    },
                ),
            )?;
            let result = source
                .journal_remote(
                    &destination,
                    &journal,
                    &targets,
                    false,
                    &cancel,
                    &mut |_| {},
                )
                .await;
            if fail_copy {
                assert!(format!("{:#}", result.unwrap_err()).contains("UploadPartCopy"));
                assert!(journal.load::<bool>("remove-0.json")?.is_none());
            } else if cancel_copy {
                assert!(result.unwrap_err().is::<TransferCancelled>());
                assert!(journal.load::<bool>("remove-0.json")?.is_none());
            } else if target_changed {
                assert!(result.unwrap_err().to_string().contains("目标对象缺失或发生变化"));
                assert!(journal.load::<bool>("remove-0.json")?.is_none());
            } else {
                result?;
                assert_eq!(
                    journal.load::<String>("copy-0.json")?.as_deref(),
                    Some("\"target\"")
                );
                assert_eq!(journal.load::<bool>("remove-0.json")?, Some(true));
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::try_join!(fixture, transfer)
        })
        .await??;
    }
    Ok(())
}

#[tokio::test]
async fn tracked_upload_body_preserves_length_and_progress_on_retry() -> Result<()> {
    let sent = Arc::new(AtomicU64::new(0));
    let slots = TransferSlots::new(1);
    let token = CancellationToken::new();
    let mut worker = slots.acquire_worker(&token, "/test".into()).await?;
    let worker_bytes = worker.track_bytes(Some(4));
    let body = tracked_body(
        ByteStream::from_static(b"test"),
        sent.clone(),
        worker_bytes.clone(),
    )
    .into_inner();
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
    assert_eq!(worker_bytes.traffic.load(Ordering::Relaxed), 8);
    assert_eq!(slots.workers().1[0].progress, Some((4, Some(4))));
    Ok(())
}

#[tokio::test]
async fn transfer_slots_track_workers_and_resize_without_interrupting_requests() -> Result<()> {
    let slots = TransferSlots::new(2);
    let token = CancellationToken::new();
    let first = slots.acquire_worker(&token, "/first".into()).await?;
    let second = slots.acquire_worker(&token, "/second".into()).await?;
    let (limit, workers) = slots.workers();
    assert_eq!(limit, 2);
    assert_eq!(
        workers
            .iter()
            .map(|worker| (worker.id, worker.label.as_str()))
            .collect::<Vec<_>>(),
        [(1, "/first"), (2, "/second")]
    );
    slots.set_limit(1);
    let queued = slots.acquire_worker(&token, "/queued".into());
    tokio::pin!(queued);
    assert!(futures_util::poll!(&mut queued).is_pending());
    drop(first);
    assert!(futures_util::poll!(&mut queued).is_pending());
    slots.set_limit(2);
    let third = queued.await?;
    assert_eq!(slots.workers().1[0].id, 1);
    assert_eq!(slots.workers().1[0].label, "/queued");

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

#[tokio::test]
async fn upload_and_download_use_independent_slots_and_byte_counters() -> Result<()> {
    let slots = TransferSlots::new(1);
    let token = CancellationToken::new();
    let mut upload = slots.acquire_worker(&token, "/upload".into()).await?;
    let waiting_upload = slots.acquire_worker(&token, "/waiting-upload".into());
    tokio::pin!(waiting_upload);
    assert!(futures_util::poll!(&mut waiting_upload).is_pending());

    let download = slots.acquire_download_worker(&token, "/download".into());
    tokio::pin!(download);
    let std::task::Poll::Ready(download) = futures_util::poll!(&mut download) else {
        panic!("the occupied upload slot must not block downloads");
    };
    let mut download = download?;
    let waiting_download = slots.acquire_download_worker(&token, "/waiting-download".into());
    tokio::pin!(waiting_download);
    assert!(futures_util::poll!(&mut waiting_download).is_pending());

    let upload_bytes = upload.track_bytes(Some(8192));
    let download_bytes = download.track_bytes(Some(4096));
    upload_bytes.record(4096, 4096);
    slots.sample_workers();
    let (_, workers) = slots.workers();
    assert!(workers[0].speed.is_some_and(|speed| speed > 0.0));
    assert_eq!(workers[1].speed, Some(0.0));
    assert_eq!(workers[0].progress, Some((4096, Some(8192))));
    assert_eq!(workers[1].progress, Some((0, Some(4096))));
    download_bytes.record(2048, 2048);
    slots.sample_workers();
    let (_, workers) = slots.workers();
    assert_eq!(workers[0].speed, Some(0.0));
    assert!(workers[1].speed.is_some_and(|speed| speed > 0.0));
    assert_eq!(workers[0].progress, Some((4096, Some(8192))));
    assert_eq!(workers[1].progress, Some((2048, Some(4096))));

    drop(download);
    assert!(futures_util::poll!(&mut waiting_download).is_ready());
    assert!(futures_util::poll!(&mut waiting_upload).is_pending());
    drop(upload);
    let _upload = waiting_upload.await?;
    assert_eq!(slots.workers().1[0].speed, None);
    Ok(())
}

#[tokio::test]
async fn directional_speed_includes_workers_that_finish_between_samples() -> Result<()> {
    let slots = TransferSlots::new(1);
    let cancel = CancellationToken::new();
    let mut upload = slots.acquire_worker(&cancel, "upload".into()).await?;
    let mut download = slots
        .acquire_download_worker(&cancel, "download".into())
        .await?;
    upload.track_bytes(Some(2048)).record(2048, 2048);
    download.track_bytes(Some(1024)).record(1024, 1024);
    drop(upload);
    drop(download);
    assert!(slots.subscribe().borrow().has_measured_workers());
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    slots.sample_workers();
    let (upload, download) = slots.direction_speeds();
    assert!(
        upload
            .zip(download)
            .is_some_and(|(up, down)| down > 0 && (up - 2 * down).abs() <= 1)
    );
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    slots.sample_workers();
    assert_eq!(slots.direction_speeds(), (None, None));
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

#[tokio::test]
async fn empty_directory_trash_uses_only_a_manifest() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let (manifest_sender, manifest_receiver) = tokio::sync::oneshot::channel();
    let fixture = async {
        let mut manifest_key = String::new();
        let mut manifest_sender = Some(manifest_sender);
        for (step, (method, status, extra, body)) in [
            (
                "GET",
                "200 OK",
                "",
                "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
            ),
            ("HEAD", "200 OK", "ETag: \"marker\"\r\n", ""),
            ("PUT", "200 OK", "", ""),
            ("DELETE", "204 No Content", "", ""),
            ("PUT", "200 OK", "", ""),
            (
                "PUT",
                "412 Precondition Failed",
                "",
                "<Error><Code>PreconditionFailed</Code></Error>",
            ),
            ("PUT", "200 OK", "", ""),
            ("DELETE", "204 No Content", "", ""),
        ]
        .into_iter()
        .enumerate()
        {
            let (socket, _) = listener.accept().await?;
            let mut socket = BufReader::new(socket);
            let mut request = String::new();
            socket.read_line(&mut request).await?;
            let mut parts = request.split_whitespace();
            assert_eq!(parts.next(), Some(method), "{request}");
            let key = parts
                .next()
                .context("missing request path")?
                .split('?')
                .next()
                .unwrap();
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                if socket.read_line(&mut line).await? == 0 || line == "\r\n" {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
            }
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: ")?.parse::<usize>().ok())
                .unwrap_or_default();
            let mut payload = vec![0; length];
            socket.read_exact(&mut payload).await?;
            match step {
                0 => assert_eq!(key.trim_end_matches('/'), "/fixture"),
                2 | 4 => {
                    assert!(key.starts_with("/fixture/.rs3fs-trash/entries/"));
                    let manifest: serde_json::Value = serde_json::from_slice(&payload)?;
                    assert_eq!(manifest["marker_only"], true);
                    assert_eq!(manifest["pending"], step == 2);
                    if step == 2 {
                        manifest_key = key.to_owned();
                    } else {
                        assert_eq!(key, manifest_key);
                        assert!(manifest_sender.take().unwrap().send(manifest).is_ok());
                    }
                }
                7 => assert_eq!(key, manifest_key),
                _ => assert_eq!(key, "/fixture/empty/"),
            }
            if step == 3 {
                assert!(headers.contains("if-match: \"marker\""));
            }
            if matches!(step, 5 | 6) {
                assert!(headers.contains("if-none-match: *"));
                assert!(payload.is_empty());
            }
            socket.get_mut().write_all(format!(
                "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/xml\r\n{extra}Content-Length: {}\r\n\r\n{body}", body.len()
            ).as_bytes()).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let workflow = async {
        let server = Server::connect(
            "us-east-1".into(),
            endpoint,
            String::new(),
            Some(Credentials::new("fixture", "fixture", None, None, "test")),
        )
        .await?;
        let store = server.open("fixture".into(), None);
        store.trash(Entry::folder("empty/".into())).await?;
        let record = serde_json::from_value(manifest_receiver.await?)?;
        let error = store.restore_trash(&record).await.unwrap_err();
        assert!(error.to_string().contains("原路径已有同名对象"));
        store.restore_trash(&record).await
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::try_join!(fixture, workflow)
    })
    .await??;
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
                .list_objects(".rs3fs-trash/objects/", false)
                .await?
                .is_empty()
        );
        assert!(
            store
                .list_objects(".rs3fs-trash/entries/", false)
                .await?
                .iter()
                .any(|entry| entry.key() == format!(".rs3fs-trash/entries/{}.json", trashed.id()))
        );
        assert!(store.list_objects(".s3fs-trash/", false).await?.is_empty());
        store.create_file(empty.key().to_owned()).await?;
        assert!(store.restore_trash(&trashed).await.is_err());
        let trashed = store
            .list_trash()
            .await?
            .into_iter()
            .find(|record| record.id() == trashed.id())
            .context("还原冲突后找不到空目录回收记录")?;
        store.delete_permanently(empty.clone()).await?;
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
        JournalState, TransferJournal, TransferOperation, TransferPaused, TransferRecord,
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
        let journal = TransferJournal::open_at(&root, upload.clone())?;
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
        let pause = journal.pause_handle();
        let paused_upload = CancellationToken::new();
        let result = store
            .transfer_file_checkpointed(
                key,
                &local,
                65 * MIB,
                None,
                super::UploadTransfer {
                    sent: Arc::new(AtomicU64::new(0)),
                    slots: &slots,
                    cancel: &paused_upload,
                    on_start: &|| assert!(pause.request(&paused_upload).unwrap()),
                },
                Some((&journal, 0)),
            )
            .await;
        assert!(matches!(result, Err(error) if error.is::<TransferCancelled>()));
        drop(journal);
        let journal = TransferJournal::open_at(&root, upload)?;
        assert!(matches!(journal.record().state, JournalState::UserPaused));
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
                overwrite: None,
            },
        );
        let journal = TransferJournal::open_at(&root, download.clone())?;
        let part = journal.part_path(&destination, 0)?;
        std::fs::write(&part, &first_part)?;
        journal.save("download-0.json", &serde_json::json!({"original": null}))?;
        journal.save(
            "range-0-0.json",
            &super::hex_digest(&Sha256::digest(&first_part)),
        )?;
        let pause = journal.pause_handle();
        let paused_download = CancellationToken::new();
        let result = journal
            .run(
                store.clone(),
                store.clone(),
                slots.clone(),
                paused_download.clone(),
                Arc::new(AtomicU64::new(0)),
                Default::default(),
                |progress| {
                    if progress == "下载字节 16777216/68157440" {
                        assert!(pause.request(&paused_download).unwrap());
                    }
                },
            )
            .await;
        assert!(matches!(result, Err(error) if error.is::<TransferPaused>()));
        assert!(part.exists());
        let journal = TransferJournal::open_at(&root, download)?;
        assert!(matches!(journal.record().state, JournalState::UserPaused));
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

#[tokio::test]
async fn sync_archive_recovers_from_unverifiable_server_copy() -> Result<()> {
    assert_trash_copy_recovery(true, true).await
}

#[tokio::test]
async fn remote_delete_waits_for_verified_recovery_copy() -> Result<()> {
    assert_trash_copy_recovery(false, true).await?;
    assert_trash_copy_recovery(false, false).await
}

async fn assert_trash_copy_recovery(archive_only: bool, relay_verified: bool) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let fixture = async {
        let mut copy_key = String::new();
        for (step, method) in [
            "PUT", "PUT", "HEAD", "HEAD", "HEAD", "DELETE", "HEAD", "HEAD", "GET", "PUT", "HEAD",
            "PUT", "DELETE", "PUT",
        ]
        .into_iter()
        .take(if !archive_only && relay_verified {
            14
        } else {
            12
        })
        .enumerate()
        {
            let (socket, _) = listener.accept().await?;
            let mut socket = BufReader::new(socket);
            let mut request = String::new();
            socket.read_line(&mut request).await?;
            let mut parts = request.split_whitespace();
            assert_eq!(parts.next(), Some(method), "{request}");
            let key = parts
                .next()
                .context("missing request path")?
                .split('?')
                .next()
                .unwrap();
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                if socket.read_line(&mut line).await? == 0 || line == "\r\n" {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
            }
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: ")?.parse::<usize>().ok())
                .unwrap_or_default();
            assert!(length < 8192);
            let mut request_body = vec![0; length];
            socket.read_exact(&mut request_body).await?;
            let (status, mut extra, body) = match step {
                0 | 11 | 13 => {
                    assert!(
                        key.starts_with("/fixture/.rs3fs-trash/entries/"),
                        "{request}"
                    );
                    let record: serde_json::Value = serde_json::from_slice(&request_body)?;
                    assert_eq!(record["archive_only"], archive_only);
                    assert_eq!(record["copy_verified"], step != 0 && relay_verified);
                    assert_eq!(
                        record["pending"],
                        step == 0 || (step == 11 && !archive_only && relay_verified)
                    );
                    assert_eq!(record["interrupted"], step != 0 && !relay_verified);
                    ("200 OK", String::new(), "")
                }
                1 => {
                    assert!(
                        key.starts_with("/fixture/.rs3fs-trash/objects/"),
                        "{request}"
                    );
                    copy_key = key.to_owned();
                    assert!(headers.contains("x-amz-copy-source-if-match: \"original\""));
                    (
                        "200 OK",
                        String::new(),
                        "<CopyObjectResult><ETag>\"copy\"</ETag></CopyObjectResult>",
                    )
                }
                2..=4 => {
                    assert_eq!(key, copy_key);
                    (
                        "200 OK",
                        "Content-Length: 2\r\nETag: \"wrong\"\r\n".into(),
                        "",
                    )
                }
                5 => {
                    assert_eq!(key, copy_key);
                    assert!(headers.contains("if-match: \"wrong\""));
                    ("204 No Content", String::new(), "")
                }
                6 => {
                    assert_eq!(key, copy_key);
                    ("404 Not Found", String::new(), "")
                }
                7 | 8 => {
                    assert_eq!(key, "/fixture/file.txt");
                    assert!(headers.contains("if-match: \"original\""));
                    (
                        "200 OK",
                        "Content-Length: 4\r\nETag: \"original\"\r\n".into(),
                        if step == 8 { "data" } else { "" },
                    )
                }
                9 => {
                    assert_eq!(key, copy_key);
                    assert!(headers.contains("if-none-match: *"));
                    assert_eq!(request_body, b"data");
                    ("200 OK", "ETag: \"relay\"\r\n".into(), "")
                }
                10 => {
                    assert_eq!(key, copy_key);
                    (
                        "200 OK",
                        format!(
                            "Content-Length: {}\r\nETag: \"relay\"\r\n",
                            if relay_verified { 4 } else { 2 }
                        ),
                        "",
                    )
                }
                12 => {
                    assert_eq!(key, "/fixture/file.txt");
                    assert!(headers.contains("if-match: \"original\""));
                    ("204 No Content", String::new(), "")
                }
                _ => unreachable!(),
            };
            if !extra.starts_with("Content-Length") {
                extra.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
            socket.get_mut().write_all(format!(
                "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/xml\r\n{extra}\r\n{body}"
            ).as_bytes()).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let archive = async {
        let server = Server::connect(
            "us-east-1".into(),
            endpoint,
            String::new(),
            Some(Credentials::new("fixture", "fixture", None, None, "test")),
        )
        .await?;
        let store = server.open("fixture".into(), None);
        let entry = Entry::file("file.txt".into(), 4, "\"original\"".into());
        let cancel = CancellationToken::new();
        let result = if archive_only {
            store.archive_in_trash(entry, &cancel).await
        } else {
            store.trash_after_copy(entry, &cancel, || Ok(())).await
        };
        if relay_verified {
            result?;
        } else {
            let error = result.unwrap_err();
            assert!(format!("{error:#}").contains("源对象尚未删除"));
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::try_join!(fixture, archive)
    })
    .await??;
    Ok(())
}
