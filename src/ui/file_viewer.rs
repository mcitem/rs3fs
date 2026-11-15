use crate::services::store::{Entry, Store, TransferSlots};
use anyhow::{Context, Result, ensure};
use gpui_kit::{Image, ImageFormat, RenderImage, SvgRenderer};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
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
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "pdf"
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
        let mut reader = if extension == "pdf" {
            let (image, _) = render_pdf_page(&path, 0, 256)?;
            image::ImageReader::with_format(
                std::io::Cursor::new(image.bytes().to_vec()),
                image::ImageFormat::Png,
            )
        } else {
            image::ImageReader::new(std::io::Cursor::new(std::fs::read(path)?))
                .with_guessed_format()?
        };
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

#[derive(Clone, Copy)]
pub(super) enum FileKind {
    Image,
    Pdf,
}

impl FileKind {
    pub(super) fn from_name(name: &str) -> Option<(Self, String)> {
        let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
        let kind = match extension.as_str() {
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "ico" | "svg" | "tif" | "tiff" => {
                Self::Image
            }
            "pdf" => Self::Pdf,
            _ => return None,
        };
        Some((kind, extension))
    }

    pub(super) fn limit(self) -> u64 {
        100 * MIB
    }
}

pub(super) async fn prepare(
    store: Store,
    entry: Entry,
    profile: Option<uuid::Uuid>,
    kind: FileKind,
    extension: String,
    svg_renderer: SvgRenderer,
    received: Arc<AtomicU64>,
    downloading: Arc<AtomicBool>,
    slots: TransferSlots,
) -> Result<PreparedPreview> {
    let directory = PreviewDirectory::new()?;
    let path = directory.path().join(format!("source.{extension}"));
    cached_source(
        &store,
        &entry,
        profile,
        &directory,
        &path,
        kind.limit(),
        &received,
        &downloading,
        &slots,
    )
    .await?;
    if matches!(kind, FileKind::Image) {
        let format = match extension.as_str() {
            "jpg" | "jpeg" => ImageFormat::Jpeg,
            "png" => ImageFormat::Png,
            "gif" => ImageFormat::Gif,
            "webp" => ImageFormat::Webp,
            "bmp" => ImageFormat::Bmp,
            "ico" => ImageFormat::Ico,
            "svg" => ImageFormat::Svg,
            "tif" | "tiff" => ImageFormat::Tiff,
            _ => unreachable!("image extension validated by FileKind"),
        };
        let bytes = tokio::fs::read(path).await?;
        let decoded = tokio::task::spawn_blocking(move || {
            Image::from_bytes(format, bytes).to_image_data(svg_renderer)
        })
        .await?;
        return decoded.map(PreparedPreview::Image).context("无法解码图片");
    }

    let bytes = tokio::fs::File::open(&path).await?;
    use tokio::io::AsyncReadExt;
    let mut header = [0; 5];
    bytes.take(5).read_exact(&mut header).await?;
    ensure!(&header == b"%PDF-", "文件内容不是有效的 PDF");
    prepare_pdf(PreparedFile {
        _directory: directory,
        page: path,
    })
    .await
}

async fn prepare_pdf(file: PreparedFile) -> Result<PreparedPreview> {
    let file = Arc::new(file);
    let render_file = file.clone();
    let (image, count) = tokio::task::spawn_blocking(move || pdf_page(render_file.path(), 0))
        .await?
        .context("无法预览 PDF")?;
    Ok(PreparedPreview::Pdf(file, image, count))
}

pub(super) enum PreparedPreview {
    Image(Arc<RenderImage>),
    Pdf(Arc<PreparedFile>, Arc<Image>, u32),
}

pub(super) fn pdf_page(path: &Path, index: u32) -> Result<(Arc<Image>, u32)> {
    render_pdf_page(path, index, 1800)
}

fn render_pdf_page(path: &Path, index: u32, resolution: u32) -> Result<(Arc<Image>, u32)> {
    use windows::{
        Data::Pdf::{PdfDocument, PdfPageRenderOptions},
        Graphics::Imaging::BitmapEncoder,
        Storage::{
            StorageFile,
            Streams::{DataReader, InMemoryRandomAccessStream},
        },
        core::HSTRING,
    };
    let _apartment = WindowsApartment::initialize(RO_INIT_MULTITHREADED)?;
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
    let document = PdfDocument::LoadFromFileAsync(&file)?.join()?;
    let count = document.PageCount()?;
    let page = document.GetPage(index)?;
    let size = page.Size()?;
    let options = PdfPageRenderOptions::new()?;
    if size.Width >= size.Height {
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
    ensure!(length <= 32 * 1024 * 1024, "PDF 页面过大");
    ensure!(
        reader.LoadAsync(length)?.join()? == length,
        "PDF 页面读取不完整"
    );
    let mut bytes = vec![0; length as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok((Arc::new(Image::from_bytes(ImageFormat::Png, bytes)), count))
}

pub(super) struct PreparedFile {
    _directory: PreviewDirectory,
    page: PathBuf,
}

impl PreparedFile {
    pub(super) fn path(&self) -> &Path {
        &self.page
    }
}
