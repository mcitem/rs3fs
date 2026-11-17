use crate::services::store::{Entry, Store, TransferSlots};
use anyhow::{Context, Result, ensure};
use gpui_kit::{Image, ImageFormat, RenderImage, SvgRenderer};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Win32::System::WinRT::{
    RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RO_INIT_TYPE, RoInitialize, RoUninitialize,
};

const MIB: u64 = 1024 * 1024;

const THUMBNAIL_SOURCE_LIMIT: u64 = 32 * MIB;

struct WindowsApartment {
    // RoUninitialize must run on the thread that called RoInitialize.
    _thread_bound: std::marker::PhantomData<*mut ()>,
}

impl WindowsApartment {
    fn initialize(mode: RO_INIT_TYPE) -> Result<Self> {
        // SAFETY: The callers supply a valid apartment mode. Each successful call
        // is paired with RoUninitialize by the guard, including S_FALSE.
        unsafe { RoInitialize(mode)? };
        Ok(Self {
            _thread_bound: std::marker::PhantomData,
        })
    }
}

impl Drop for WindowsApartment {
    fn drop(&mut self) {
        // SAFETY: The guard is only constructed after successful initialization
        // and cannot be moved to another thread.
        unsafe { RoUninitialize() };
    }
}

pub(super) fn thumbnail_key(
    store: &Store,
    entry: &Entry,
    profile: Option<uuid::Uuid>,
) -> Option<String> {
    let (_, extension) = entry.name().rsplit_once('.')?;
    if entry.directory()
        || entry.size() < 0
        || entry.size() as u64 > THUMBNAIL_SOURCE_LIMIT
        || !matches!(
            extension.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff"
        )
    {
        return None;
    }
    cache_key(store, entry, profile)
}

pub(super) async fn thumbnail(
    store: Store,
    entry: Entry,
    profile: Option<uuid::Uuid>,
    key: &str,
    slots: TransferSlots,
) -> Result<Arc<Image>> {
    let root = cache_root()?;
    let cached = root.join(key).join("thumbnail-v1.png");
    if let Ok(bytes) = tokio::fs::read(&cached).await {
        return Ok(Arc::new(Image::from_bytes(ImageFormat::Png, bytes)));
    }
    let directory = PreviewDirectory::new()?;
    let extension = entry
        .name()
        .rsplit_once('.')
        .context("缺少文件扩展名")?
        .1
        .to_ascii_lowercase();
    let path = directory.path().join(format!("source.{extension}"));
    cached_source(
        &store,
        &entry,
        profile,
        &directory,
        &path,
        THUMBNAIL_SOURCE_LIMIT,
        &AtomicU64::new(0),
        &AtomicBool::new(false),
        &slots,
    )
    .await?;
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(96 * MIB);
        limits.max_image_width = Some(16_384);
        limits.max_image_height = Some(16_384);
        let mut reader = image::ImageReader::new(std::io::Cursor::new(std::fs::read(path)?))
            .with_guessed_format()?;
        reader.limits(limits);
        use image::ImageDecoder;
        let mut decoder = reader.into_decoder()?;
        let orientation = decoder.orientation()?;
        let mut thumbnail = image::DynamicImage::from_decoder(decoder)?.thumbnail(256, 256);
        thumbnail.apply_orientation(orientation);
        let mut png = std::io::Cursor::new(Vec::new());
        thumbnail.write_to(&mut png, image::ImageFormat::Png)?;
        let bytes = png.into_inner();
        let _ = (|| -> Result<()> {
            use std::io::Write;
            let _guard = lock_cache(&root)?;
            let folder = cached.parent().context("缺少缩略图缓存目录")?;
            std::fs::create_dir_all(folder)?;
            let mut temp = tempfile::NamedTempFile::new_in(folder)?;
            temp.write_all(&bytes)?;
            temp.persist_noclobber(cached)?;
            Ok(())
        })();
        Ok(bytes)
    })
    .await??;
    Ok(Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
}

fn cache_key(store: &Store, entry: &Entry, profile: Option<uuid::Uuid>) -> Option<String> {
    let etag = entry
        .etag()
        .filter(|etag| !etag.is_empty() && *etag != "\"\"")?;
    let identity = serde_json::to_vec(&(
        profile,
        store.preview_origin(),
        entry.key(),
        etag,
        entry.size(),
    ))
    .ok()?;
    Some(format!(
        "preview-object-{}",
        crate::services::store::hex_digest(&Sha256::digest(identity))
    ))
}

async fn cached_source(
    store: &Store,
    entry: &Entry,
    profile: Option<uuid::Uuid>,
    directory: &PreviewDirectory,
    path: &Path,
    limit: u64,
    received: &AtomicU64,
    downloading: &AtomicBool,
    slots: &TransferSlots,
) -> Result<()> {
    let cached = cache_key(store, entry, profile)
        .map(|key| directory.path().with_file_name(key).join("object"));
    if let Some(cached) = &cached
        && let Ok(metadata) = tokio::fs::metadata(cached).await
        && metadata.is_file()
        && metadata.len() <= limit
        && i64::try_from(metadata.len()).ok() == Some(entry.size())
        && tokio::fs::hard_link(cached, path).await.is_ok()
    {
        received.store(metadata.len(), Ordering::Relaxed);
        return Ok(());
    }
    downloading.store(true, Ordering::Relaxed);
    let result = store
        .download_preview_with_workers(entry, path, limit, received, slots)
        .await;
    downloading.store(false, Ordering::Relaxed);
    result?;
    if let Some(cached) = cached
        && i64::try_from(received.load(Ordering::Relaxed)).ok() == Some(entry.size())
    {
        // Publish a complete file atomically. Each active viewer owns its own link.
        let path = path.to_owned();
        let _ = tokio::task::spawn_blocking(move || publish_cached_file(&path, &cached)).await;
    }
    Ok(())
}

fn publish_cached_file(path: &Path, cached: &Path) -> Result<()> {
    let parent = cached.parent().context("缓存路径无效")?;
    let root = parent.parent().context("缓存目录无效")?;
    let _guard = lock_cache(root)?;
    std::fs::create_dir_all(parent)?;
    match std::fs::hard_link(path, cached) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn prepare_text(
    store: Store,
    entry: Entry,
    profile: Option<uuid::Uuid>,
    limit: u64,
    received: Arc<AtomicU64>,
    downloading: Arc<AtomicBool>,
    slots: TransferSlots,
) -> Result<(String, crate::services::store::TextEncoding)> {
    let directory = PreviewDirectory::new()?;
    let path = directory.path().join("source.txt");
    cached_source(
        &store,
        &entry,
        profile,
        &directory,
        &path,
        limit,
        &received,
        &downloading,
        &slots,
    )
    .await?;
    crate::services::store::decode_editable_text(tokio::fs::read(path).await?)
}

struct PreviewDirectory {
    _lease: std::fs::File,
    directory: tempfile::TempDir,
}

pub(super) struct ExternalFile {
    directory: PreviewDirectory,
    path: PathBuf,
}

pub(super) fn prepare_local_sync_version(
    job: &crate::services::sync::SyncJob,
    item: &crate::services::sync::PlanItem,
) -> Result<ExternalFile> {
    let directory = PreviewDirectory::new()?;
    let name = item.path().rsplit('/').next().context("缺少文件名")?;
    let path = directory.path().join(format!("本地-{name}"));
    item.copy_local_version(job, &path)?;
    Ok(ExternalFile { directory, path })
}

pub(super) async fn prepare_external(
    store: Store,
    entry: Entry,
    profile: Option<uuid::Uuid>,
    received: Arc<AtomicU64>,
    downloading: Arc<AtomicBool>,
    slots: TransferSlots,
) -> Result<ExternalFile> {
    let directory = PreviewDirectory::new()?;
    let name: String = entry
        .name()
        .chars()
        .map(|ch| {
            if ch.is_control() || "<>:\"/\\|?*".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let name = name.trim_end_matches([' ', '.']);
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
    let mut name = format!("S3-{}", stem.chars().take(80).collect::<String>());
    if !extension.is_empty() {
        name.push('.');
        name.extend(extension.chars().take(24));
    }
    let path = directory.path().join(&name);
    cached_source(
        &store,
        &entry,
        profile,
        &directory,
        &path,
        i64::MAX as u64,
        &received,
        &downloading,
        &slots,
    )
    .await?;
    // External editors may write to the file. Give them their own copy of cached content.
    let editable = directory.path().join("open");
    tokio::fs::create_dir(&editable).await?;
    let copy = editable.join(name);
    tokio::fs::copy(&path, &copy).await?;
    tokio::fs::remove_file(path).await?;
    Ok(ExternalFile {
        directory,
        path: copy,
    })
}

fn associated_handlers(
    extension: &str,
    filter: windows::Win32::UI::Shell::ASSOC_FILTER,
) -> Result<Vec<windows::Win32::UI::Shell::IAssocHandler>> {
    use windows::{
        Win32::UI::Shell::SHAssocEnumHandlers,
        core::{HSTRING, PCWSTR},
    };
    let extension = HSTRING::from(format!(".{extension}"));
    let handlers = unsafe { SHAssocEnumHandlers(PCWSTR(extension.as_ptr()), filter)? };
    let mut result = Vec::new();
    loop {
        let mut item = [None];
        unsafe { handlers.Next(&mut item, None)? };
        let [Some(handler)] = item else { break };
        result.push(handler);
    }
    Ok(result)
}

fn shell_string(value: windows::core::PWSTR) -> Option<String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    if value.is_null() {
        return None;
    }
    let text = unsafe { value.to_string().ok() };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    text.filter(|text| !text.is_empty())
}

pub(super) fn open_with_apps(name: &str) -> Vec<(String, String)> {
    use windows::Win32::UI::Shell::{ASSOC_FILTER_NONE, ASSOC_FILTER_RECOMMENDED};
    let Some((_, extension)) = name.rsplit_once('.') else {
        return Vec::new();
    };
    let handlers = associated_handlers(extension, ASSOC_FILTER_RECOMMENDED).unwrap_or_default();
    let handlers = if handlers.is_empty() {
        associated_handlers(extension, ASSOC_FILTER_NONE).unwrap_or_default()
    } else {
        handlers
    };
    let mut seen = std::collections::HashSet::new();
    handlers
        .into_iter()
        .filter_map(|handler| {
            let id = shell_string(unsafe { handler.GetName().ok()? })?;
            let label = shell_string(unsafe { handler.GetUIName().ok()? })?;
            seen.insert(id.clone()).then_some((id, label))
        })
        .collect()
}

impl ExternalFile {
    pub(super) fn choose_application(self, application: Option<String>) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::{OAIF_EXEC, OPENASINFO, SHOpenWithDialog};

        let _apartment = WindowsApartment::initialize(RO_INIT_SINGLETHREADED)?;
        let path: Vec<u16> = self.path.as_os_str().encode_wide().chain([0]).collect();
        if let Some(application) = application {
            use windows::{
                Win32::{
                    System::Com::{IBindCtx, IDataObject},
                    UI::Shell::{
                        ASSOC_FILTER_NONE, BHID_DataObject, IShellItem, SHCreateItemFromParsingName,
                    },
                },
                core::PCWSTR,
            };
            let extension = self
                .path
                .extension()
                .and_then(|value| value.to_str())
                .context("缺少文件扩展名")?;
            let handler = associated_handlers(extension, ASSOC_FILTER_NONE)?
                .into_iter()
                .find(|handler| {
                    unsafe { handler.GetName() }
                        .ok()
                        .and_then(shell_string)
                        .as_deref()
                        == Some(&application)
                })
                .context("所选应用已不可用，请重新选择打开方式")?;
            unsafe {
                let item: IShellItem =
                    SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None::<&IBindCtx>)?;
                let data: IDataObject = item.BindToHandler(None::<&IBindCtx>, &BHID_DataObject)?;
                handler.Invoke(&data).context("无法用所选应用打开文件")?;
            }
            let _ = self.directory.directory.keep();
            return Ok(());
        }
        let info = OPENASINFO {
            pcszFile: path.as_ptr(),
            pcszClass: std::ptr::null(),
            oaifInFlags: OAIF_EXEC,
        };
        let result = unsafe { SHOpenWithDialog(std::ptr::null_mut(), &info) };
        if result == 0x800704C7u32 as i32 {
            return Ok(());
        }
        windows::core::HRESULT(result)
            .ok()
            .context("无法打开应用选择器")?;
        // The chosen application may open the copy after the picker returns.
        let _ = self.directory.directory.keep();
        Ok(())
    }
}

impl PreviewDirectory {
    fn new() -> Result<Self> {
        let root = cache_root()?;
        let _guard = lock_cache(&root)?;
        let directory = tempfile::Builder::new()
            .prefix("preview-")
            .tempdir_in(root)?;
        let lease = std::fs::File::create_new(directory.path().join("active.lock"))?;
        lease.lock()?;
        Ok(Self {
            _lease: lease,
            directory,
        })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }
}

fn cache_root() -> Result<PathBuf> {
    if let Some(directory) =
        std::env::var_os("S3FS_PREVIEW_CACHE_DIR").filter(|path| !path.is_empty())
    {
        return Ok(PathBuf::from(directory));
    }
    Ok(crate::services::settings::ConnectionSettings::path()?.with_file_name("preview-cache"))
}

fn lock_cache(root: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(root)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("cache.lock"))?;
    lock.lock()?;
    Ok(lock)
}

pub(super) struct CacheUsage {
    pub(super) total: u64,
    pub(super) removed: u64,
    pub(super) active: bool,
    pub(super) failed: bool,
}

pub(super) fn cache_usage(clear: bool) -> Result<CacheUsage> {
    let root = cache_root()?;
    scan_cache(&root, clear)
}

fn scan_cache(root: &Path, clear: bool) -> Result<CacheUsage> {
    let _guard = lock_cache(root)?;
    let mut usage = CacheUsage {
        total: 0,
        removed: 0,
        active: false,
        failed: false,
    };
    let directories = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(usage),
        Err(error) => return Err(error.into()),
    };
    for directory in directories {
        let directory = directory?;
        if !directory.file_type()?.is_dir()
            || !directory
                .file_name()
                .to_string_lossy()
                .starts_with("preview-")
        {
            continue;
        }
        let path = directory.path();
        let size = || {
            walkdir::WalkDir::new(&path)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .filter_map(|entry| entry.metadata().ok())
                .map(|metadata| metadata.len())
                .fold(0u64, u64::saturating_add)
        };
        let before = size();
        if clear {
            let lease = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.join("active.lock"));
            let removable = match lease {
                Ok(file) => match file.try_lock() {
                    Ok(()) => true,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        usage.active = true;
                        false
                    }
                    Err(_) => {
                        usage.failed = true;
                        false
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => {
                    usage.failed = true;
                    false
                }
            };
            if removable && let Err(error) = std::fs::remove_dir_all(&path) {
                usage.failed |= error.kind() != std::io::ErrorKind::NotFound;
            }
            let remaining = size();
            usage.removed = usage
                .removed
                .saturating_add(before.saturating_sub(remaining));
            usage.total = usage.total.saturating_add(remaining);
        } else {
            usage.total = usage.total.saturating_add(before);
        }
    }
    Ok(usage)
}

#[cfg(test)]
#[test]
fn cache_cleanup_preserves_active_previews() -> Result<()> {
    let root = tempfile::tempdir()?;
    let active = root.path().join("preview-active");
    let stale = root.path().join("preview-stale");
    std::fs::create_dir(&active)?;
    std::fs::create_dir(&stale)?;
    let lease = std::fs::File::create_new(active.join("active.lock"))?;
    lease.lock()?;
    std::fs::write(active.join("source.pdf"), b"active")?;
    std::fs::write(stale.join("source.pdf"), b"stale")?;
    assert_eq!(scan_cache(root.path(), false)?.total, 11);
    let usage = scan_cache(root.path(), true)?;
    assert_eq!(usage.total, 6);
    assert_eq!(usage.removed, 5);
    assert!(usage.active);
    assert!(!usage.failed);
    assert!(!stale.exists());
    drop(lease);
    assert_eq!(scan_cache(root.path(), true)?.total, 0);
    Ok(())
}

pub(super) const DOCUMENT_PREVIEW_LIMIT: u64 = 100 * MIB;

pub(super) fn image_format(name: &str) -> Option<(ImageFormat, String)> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    let format = match extension.as_str() {
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "png" => ImageFormat::Png,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        "ico" => ImageFormat::Ico,
        "svg" => ImageFormat::Svg,
        "tif" | "tiff" => ImageFormat::Tiff,
        _ => return None,
    };
    Some((format, extension))
}

pub(super) async fn prepare_image(
    store: Store,
    entry: Entry,
    profile: Option<uuid::Uuid>,
    format: ImageFormat,
    extension: String,
    svg_renderer: SvgRenderer,
    received: Arc<AtomicU64>,
    downloading: Arc<AtomicBool>,
    slots: TransferSlots,
) -> Result<Arc<RenderImage>> {
    let directory = PreviewDirectory::new()?;
    let path = directory.path().join(format!("source.{extension}"));
    cached_source(
        &store,
        &entry,
        profile,
        &directory,
        &path,
        DOCUMENT_PREVIEW_LIMIT,
        &received,
        &downloading,
        &slots,
    )
    .await?;
    let bytes = tokio::fs::read(path).await?;
    tokio::task::spawn_blocking(move || {
        Image::from_bytes(format, bytes).to_image_data(svg_renderer)
    })
    .await?
    .context("无法解码图片")
}

pub(super) fn is_pdf(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("pdf"))
}

pub(super) async fn prepare_pdf(
    store: Store,
    entry: Entry,
    profile: Option<uuid::Uuid>,
    received: Arc<AtomicU64>,
    downloading: Arc<AtomicBool>,
    slots: TransferSlots,
) -> Result<Arc<PdfFile>> {
    let directory = PreviewDirectory::new()?;
    let path = directory.path().join("source.pdf");
    cached_source(
        &store,
        &entry,
        profile,
        &directory,
        &path,
        DOCUMENT_PREVIEW_LIMIT,
        &received,
        &downloading,
        &slots,
    )
    .await?;
    tokio::task::spawn_blocking(move || PdfFile::open(directory, &path))
        .await?
        .context("无法预览 PDF")
}

pub(super) struct PdfFile {
    document: PdfDocument,
    dimensions: Vec<(f32, f32)>,
    _directory: PreviewDirectory,
}

impl PdfFile {
    fn open(directory: PreviewDirectory, path: &Path) -> Result<Arc<Self>> {
        use windows::{Storage::StorageFile, core::HSTRING};

        let _apartment = WindowsApartment::initialize(RO_INIT_MULTITHREADED)?;
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
        let document = PdfDocument::LoadFromFileAsync(&file)?.join()?;
        let count = document.PageCount()?;
        ensure!(count != 0, "PDF 没有页面");
        let dimensions = (0..count)
            .map(|index| {
                let page = document.GetPage(index)?;
                let size = page.Size()?;
                ensure!(
                    size.Width.is_finite()
                        && size.Width > 0.
                        && size.Height.is_finite()
                        && size.Height > 0.,
                    "PDF 页面大小无效"
                );
                Ok((size.Width, size.Height))
            })
            .collect::<Result<_>>()?;
        Ok(Arc::new(Self {
            document,
            dimensions,
            _directory: directory,
        }))
    }

    pub(super) fn dimensions(&self) -> &[(f32, f32)] {
        &self.dimensions
    }

    pub(super) fn render_page(&self, index: usize, resolution: u32) -> Result<Arc<Image>> {
        use windows::{
            Graphics::Imaging::BitmapEncoder,
            Storage::Streams::{DataReader, InMemoryRandomAccessStream},
        };
        let _apartment = WindowsApartment::initialize(RO_INIT_MULTITHREADED)?;
        let page = self.document.GetPage(u32::try_from(index)?)?;
        let (width, height) = self.dimensions[index];
        let options = PdfPageRenderOptions::new()?;
        if width >= height {
            options.SetDestinationWidth(resolution)?;
        } else {
            options.SetDestinationHeight(resolution)?;
        }
        options.SetBitmapEncoderId(BitmapEncoder::PngEncoderId()?)?;
        let stream = InMemoryRandomAccessStream::new()?;
        page.RenderWithOptionsToStreamAsync(&stream, &options)?
            .join()?;
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
        let length = u32::try_from(stream.Size()?)?;
        ensure!(u64::from(length) <= 32 * MIB, "PDF 页面过大");
        ensure!(
            reader.LoadAsync(length)?.join()? == length,
            "PDF 页面读取不完整"
        );
        let mut bytes = vec![0; length as usize];
        reader.ReadBytes(&mut bytes)?;
        Ok(Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
    }
}

pub(super) struct OfficeSource {
    pub(super) path: PathBuf,
    _directory: PreviewDirectory,
}

pub(super) async fn prepare_office(
    store: Store,
    entry: Entry,
    profile: Option<uuid::Uuid>,
    extension: &'static str,
    received: Arc<AtomicU64>,
    downloading: Arc<AtomicBool>,
    slots: TransferSlots,
) -> Result<super::office_document::OfficeDocument> {
    let directory = PreviewDirectory::new()?;
    let normalized = match extension {
        ".doc" => ".docx",
        ".ppt" => ".pptx",
        other => other,
    };
    let input = directory.path().join(format!("source{extension}"));
    let path = directory.path().join(format!("source{normalized}"));
    let converted = (normalized != extension)
        .then(|| cache_key(&store, &entry, profile))
        .flatten()
        .map(|key| {
            directory
                .path()
                .with_file_name(key)
                .join(format!("office-v1{normalized}"))
        });
    let cache_hit = if let Some(cached) = &converted {
        tokio::fs::hard_link(cached, &path).await.is_ok()
    } else {
        false
    };
    if !cache_hit {
        cached_source(
            &store,
            &entry,
            profile,
            &directory,
            &input,
            DOCUMENT_PREVIEW_LIMIT,
            &received,
            &downloading,
            &slots,
        )
        .await?;
        let native_error = if extension == ".doc" {
            let input = input.clone();
            match tokio::task::spawn_blocking(move || {
                super::office_document::OfficeDocument::legacy_word(&input)
            })
            .await?
            {
                Ok(document) => return Ok(document),
                Err(error) => Some(error),
            }
        } else {
            None
        };
        if normalized != extension {
            let converter = libreoffice_path().ok_or_else(|| {
                native_error.unwrap_or_else(|| {
                    anyhow::anyhow!("PPT 预览需要 LibreOffice。也可使用「打开方式」。")
                })
            })?;
            convert_legacy_office(&converter, &input, &path, normalized, directory.path()).await?;
            if let Some(cached) = converted {
                let output = path.clone();
                let _ = tokio::task::spawn_blocking(move || publish_cached_file(&output, &cached))
                    .await;
            }
        }
    }
    let source = OfficeSource {
        path,
        _directory: directory,
    };
    tokio::task::spawn_blocking(move || {
        super::office_document::OfficeDocument::open(source, normalized)
    })
    .await?
    .context("无法预览 Office 文件")
}

fn libreoffice_path() -> Option<PathBuf> {
    use windows_registry::{CURRENT_USER, LOCAL_MACHINE};
    [CURRENT_USER, LOCAL_MACHINE]
        .into_iter()
        .filter_map(|root| root.open(r"SOFTWARE\LibreOffice\UNO\InstallPath").ok())
        .filter_map(|key| key.get_string("").ok().map(PathBuf::from))
        .chain(
            ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(|base| PathBuf::from(base).join("LibreOffice").join("program")),
        )
        .map(|directory| directory.join("soffice.exe"))
        .find(|executable| executable.is_file())
}

async fn convert_legacy_office(
    executable: &Path,
    input: &Path,
    output: &Path,
    extension: &str,
    directory: &Path,
) -> Result<()> {
    let profile = directory.join("libreoffice").join("user");
    tokio::fs::create_dir_all(&profile).await?;
    // A fresh profile with no trusted macro locations, independent of the user's settings.
    tokio::fs::write(
        profile.join("registrymodifications.xcu"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<oor:items xmlns:oor="http://openoffice.org/2001/registry">
  <item oor:path="/org.openoffice.Office.Common/Security/Scripting">
    <prop oor:name="MacroSecurityLevel" oor:op="fuse"><value>3</value></prop>
  </item>
  <item oor:path="/org.openoffice.Office.Writer/Content/Update">
    <prop oor:name="Link" oor:op="fuse"><value>0</value></prop>
  </item>
</oor:items>"#,
    )
    .await?;
    let url = url::Url::from_directory_path(profile.parent().context("缺少 LibreOffice 配置目录")?)
        .map_err(|_| anyhow::anyhow!("无法创建 LibreOffice 配置路径"))?;
    let filter = match extension {
        ".docx" => "docx:Office Open XML Text",
        ".pptx" => "pptx:Impress MS PowerPoint 2007 XML",
        _ => anyhow::bail!("不支持的 Office 转换格式"),
    };
    let mut child = tokio::process::Command::new(executable)
        .creation_flags(0x08000000)
        .arg(format!("-env:UserInstallation={url}"))
        .args([
            "--headless",
            "--norestore",
            "--convert-to",
            filter,
            "--outdir",
        ])
        .arg(directory)
        .arg(input)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("无法启动 LibreOffice 预览转换")?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
        .await
        .context("LibreOffice 转换超时，可使用「打开方式」打开")?;
    let status = result.context("无法等待 LibreOffice 转换")?;
    let length = tokio::fs::metadata(output)
        .await
        .map_or(0, |metadata| metadata.len());
    ensure!(
        status.success() && length > 0,
        "LibreOffice 无法转换此文件，可使用「打开方式」打开"
    );
    ensure!(
        length <= DOCUMENT_PREVIEW_LIMIT,
        "转换后的文件超过预览大小上限"
    );
    Ok(())
}
