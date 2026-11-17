//! Launch rs3fs against a disposable local S3 copy of the source checkout.

#[cfg(not(windows))]
fn main() {
    eprintln!("The rs3fs desktop demo requires Windows.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_demo::run()
}

#[cfg(windows)]
mod windows_demo {
    use anyhow::{Context, Result, ensure};
    use aws_sdk_s3::{
        Client,
        config::{Credentials, Region, RequestChecksumCalculation},
        primitives::ByteStream,
    };
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        io::Read,
        net::TcpListener,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::Duration,
    };
    use uuid::Uuid;

    const BUCKET: &str = "rs3fs-demo";
    const DEMO_DIR: &str = BUCKET;
    const REGION: &str = "us-east-1";
    const ACCESS_KEY: &str = "rs3fs-local-demo";
    const RELEASE: &str =
        "https://github.com/rustfs/rustfs/releases/download/1.0.5/rustfs-windows-x86_64-v1.0.5.zip";
    const RELEASE_SHA256: &str = "4df180336d9774ab63eeb86b99935e697967018962bbba876e67f2932fd05dff";

    #[tokio::main]
    pub(super) async fn run() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
        let executable = build_app(&root)?;
        let work = root.join("target").join(DEMO_DIR).join("work");
        fs::create_dir_all(work.join("downloads"))?;
        let session = tempfile::Builder::new()
            .prefix("session-")
            .tempdir_in(&work)?;
        let storage = session.path().join("storage");
        fs::create_dir_all(&storage)?;
        let secret = Uuid::new_v4().simple().to_string();
        let rustfs = extract_rustfs(&root, session.path())?;
        let address = TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .to_string();
        let endpoint = format!("http://{address}");
        let mut command = tokio::process::Command::new(rustfs);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("RUSTFS_") || name.starts_with("OTEL_") {
                command.env_remove(key);
            }
        }
        let mut server = command
            .current_dir(session.path())
            .args(["server", "--address", &address, "--region", REGION])
            .arg(&storage)
            .env("RUSTFS_ACCESS_KEY", ACCESS_KEY)
            .env("RUSTFS_SECRET_KEY", &secret)
            .env("RUSTFS_CONSOLE_ENABLE", "false")
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("could not start RustFS")?;
        let result = tokio::select! {
            result = run_app(&root, &work, session.path(), &executable, &endpoint, &secret) => result,
            status = server.wait() => Err(anyhow::anyhow!("RustFS exited early ({})", status?)),
        };
        if server.try_wait()?.is_none() {
            server.kill().await.context("could not stop RustFS")?;
        }
        result
    }

    fn extract_rustfs(root: &Path, session: &Path) -> Result<PathBuf> {
        ensure!(cfg!(target_arch = "x86_64"), "this demo needs Windows x64");
        let cache = root.join("target").join(DEMO_DIR);
        fs::create_dir_all(&cache)?;
        let archive = cache.join("rustfs-windows-x86_64-v1.0.5.zip");
        if !archive.is_file() {
            println!("Downloading RustFS 1.0.5 from GitHub Releases...");
            let download = tempfile::NamedTempFile::new_in(&cache)?.into_temp_path();
            let mut curl = Command::new("curl.exe");
            if let Some(proxy) = windows_proxy(RELEASE)? {
                curl.env("HTTPS_PROXY", proxy);
            }
            let status = curl
                .args([
                    "--disable",
                    "--fail",
                    "--location",
                    "--proto",
                    "=https",
                    "--max-time",
                    "300",
                    "--output",
                ])
                .arg(&download)
                .arg(RELEASE)
                .status()
                .context("could not run curl.exe")?;
            ensure!(status.success(), "RustFS download failed ({status})");
            verify_archive(&download)?;
            if let Err(error) = download.persist_noclobber(&archive)
                && error.error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(error.into());
            }
        }
        verify_archive(&archive)?;
        let status = Command::new("tar.exe")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(session)
            .arg("rustfs.exe")
            .status()
            .context("could not run tar.exe")?;
        ensure!(status.success(), "could not extract RustFS ({status})");
        Ok(session.join("rustfs.exe"))
    }

    fn windows_proxy(url: &str) -> Result<Option<String>> {
        // Let curl handle explicit proxy and bypass environment variables as usual.
        if ["HTTPS_PROXY", "ALL_PROXY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        {
            return Ok(None);
        }

        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                r#"$ErrorActionPreference = 'Stop'
$url = [Uri]$env:RS3FS_DOWNLOAD_URL
$proxy = [System.Net.WebRequest]::GetSystemWebProxy().GetProxy($url)
if ($null -ne $proxy -and $proxy -ne $url) { $proxy.AbsoluteUri }"#,
            ])
            .env("RS3FS_DOWNLOAD_URL", url)
            .output()
            .context("could not read the Windows proxy settings")?;
        ensure!(
            output.status.success(),
            "could not resolve the Windows proxy for the RustFS download ({})",
            output.status
        );
        let proxy = String::from_utf8(output.stdout)?.trim().to_owned();
        Ok((!proxy.is_empty()).then_some(proxy))
    }

    fn verify_archive(archive: &Path) -> Result<()> {
        let mut file = fs::File::open(archive)?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let digest: String = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        ensure!(
            digest == RELEASE_SHA256,
            "RustFS archive failed SHA-256 verification: {}",
            archive.display()
        );
        Ok(())
    }

    async fn run_app(
        root: &Path,
        work: &Path,
        session: &Path,
        executable: &Path,
        endpoint: &str,
        secret: &str,
    ) -> Result<()> {
        let count = seed_bucket(root, endpoint, secret).await?;
        let profile = session.join("profile");
        let config = profile.join("s3fs");
        fs::create_dir_all(&config)?;
        fs::write(
            config.join("connection.json"),
            serde_json::to_vec(&serde_json::json!({
                "bucket": BUCKET,
                "region": REGION,
                "endpoint": endpoint,
            }))?,
        )?;
        // A running Windows executable is locked. Leave the Cargo artifact free to rebuild.
        let demo_app = session.join(concat!(env!("CARGO_PKG_NAME"), ".exe"));
        fs::copy(executable, &demo_app)?;
        let mut app = tokio::process::Command::new(demo_app);
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("AWS_")
            {
                app.env_remove(key);
            }
        }
        let mut child = app
            .current_dir(work)
            .env("LOCALAPPDATA", &profile)
            .env("S3FS_DOWNLOAD_DIR", work.join("downloads"))
            .env("S3FS_PREVIEW_CACHE_DIR", work.join("preview-cache"))
            .env("AWS_CONFIG_FILE", profile.join("aws-config"))
            .env(
                "AWS_SHARED_CREDENTIALS_FILE",
                profile.join("aws-credentials"),
            )
            .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
            .env("AWS_SECRET_ACCESS_KEY", secret)
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("NO_PROXY", "127.0.0.1")
            .kill_on_drop(true)
            .spawn()
            .context("could not start rs3fs")?;
        println!("{endpoint} / {BUCKET} ({count} files). Exit from the tray or press Ctrl+C.");

        tokio::select! {
            status = child.wait() => {
                let status = status.context("could not wait for rs3fs")?;
                ensure!(status.success(), "rs3fs exited with {status}");
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("could not listen for Ctrl+C")?;
                child.kill().await.context("could not stop rs3fs")?;
            }
        }
        Ok(())
    }

    fn build_app(root: &Path) -> Result<PathBuf> {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(cargo)
            .current_dir(root)
            .args([
                "build",
                "--locked",
                "--bin",
                env!("CARGO_PKG_NAME"),
                "--message-format=json-render-diagnostics",
            ])
            .stderr(Stdio::inherit())
            .output()
            .context("could not run Cargo")?;
        ensure!(
            output.status.success(),
            "could not build rs3fs ({})",
            output.status
        );
        String::from_utf8(output.stdout)?
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|message| {
                message["reason"] == "compiler-artifact"
                    && message["target"]["name"] == env!("CARGO_PKG_NAME")
            })
            .filter_map(|message| message["executable"].as_str().map(PathBuf::from))
            .last()
            .context("Cargo did not report the rs3fs executable")
    }

    async fn seed_bucket(root: &Path, endpoint: &str, secret: &str) -> Result<usize> {
        let output = Command::new("git")
            .current_dir(root)
            .args(["ls-files", "-z", "--deduplicate"])
            .output()
            .context("Git is required to list the files in the source checkout")?;
        ensure!(
            output.status.success(),
            "could not list Git-tracked source files"
        );
        let tracked = String::from_utf8(output.stdout)?;
        let samples = [
            "docs/demo-images/blue.png",
            "docs/demo-images/green.png",
            "docs/demo-images/orange.png",
            "docs/demo-images/purple.png",
        ];
        let mut files: Vec<_> = tracked.split_terminator('\0').chain(samples).collect();
        files.sort_unstable();
        files.dedup();

        let client = Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .region(Region::new(REGION))
                .endpoint_url(endpoint)
                .force_path_style(true)
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .credentials_provider(Credentials::new(ACCESS_KEY, secret, None, None, "demo"))
                .build(),
        );
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if client.list_buckets().send().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .context("RustFS did not become ready in 30 seconds")?;
        client
            .create_bucket()
            .bucket(BUCKET)
            .send()
            .await
            .context("could not create demo bucket")?;
        let mut count = 0;
        for key in files {
            let relative: PathBuf = key.split('/').collect();
            let source = root.join(&relative);
            if !source.is_file() || source.canonicalize()? != source {
                continue;
            }
            client
                .put_object()
                .bucket(BUCKET)
                .key(key)
                .body(ByteStream::from_path(&source).await?)
                .send()
                .await
                .with_context(|| format!("could not upload {key} into the demo bucket"))?;
            count += 1;
        }
        let images = client
            .list_objects_v2()
            .bucket(BUCKET)
            .prefix("docs/demo-images/")
            .delimiter("/")
            .send()
            .await
            .context("could not list demo images")?;
        for key in samples {
            ensure!(
                images.contents().iter().any(|object| {
                    object.key() == Some(key) && object.e_tag().is_some_and(|etag| !etag.is_empty())
                }),
                "demo image {key} is missing or its listing contains no ETag",
            );
        }
        Ok(count)
    }
}
