use crate::services::{
    settings::DisplayTimeZone,
    sync::ConflictPolicy,
    sync_service::{Outcome, SyncService},
    transfer::TransferState,
};
use crate::services::{
    settings::{
        BYTES_PER_MB, BucketQuotas, ConnectionProfile, ConnectionProfiles, ConnectionSettings,
        DEFAULT_PREVIEW_LIMIT_MB, DotEntrySettings, MAX_TRANSFER_PARALLELISM, PreviewSettings,
        TransferSettings, TrashRetention, write_settings,
    },
    store::{Bucket, Entry, Server, Store, TransferSlots, TrashEntry, child_key, parent_prefix},
};
use anyhow::{Context as _, Result, ensure};
use gpui_kit::{
    component::{
        ActiveTheme, Disableable, Icon, Selectable, Sizable, WindowExt,
        button::{Button, ButtonVariant, ButtonVariants},
        command::{Command, CommandItem, CommandState},
        dialog::DialogButtonProps,
        form::{Field, Form},
        input::{Editor, EditorState, Input, InputEvent, InputState},
        menu::{ContextMenuExt, PopupMenu, PopupMenuItem},
        progress::Progress,
        spinner::Spinner,
        status_bar::StatusBar,
        switch::Switch,
    },
    *,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

mod connection;
mod durable_transfer;
mod editing;
mod entry_menu;
#[cfg(windows)]
mod file_clipboard;
mod file_grid;
mod file_list;
mod keyboard;
mod navigation;
mod output_panel;
mod preferences;
mod preview;
mod remote_picker;
mod search;
mod sidebar;
mod status_bar;
mod storage;
mod sync_settings;
mod sync_tasks;
mod task_panel;
mod transfers;
mod trash;

use editing::EditMessage;
use file_list::file_icon;
use gpui_kit::{assets::IconName, prelude::FluentBuilder};
use navigation::is_dot_path;

const LOG_HISTORY_LIMIT: usize = 500;
const FILE_NAME_LINE_HEIGHT: Rems = Rems(1.25);
struct LogEntry {
    time: chrono::DateTime<chrono::Utc>,
    failed: bool,
    transfer_id: Option<u64>,
    message: SharedString,
}

impl LogEntry {
    fn new(transfer_id: Option<u64>, failed: bool, message: impl Into<SharedString>) -> Self {
        Self {
            time: chrono::Utc::now(),
            failed,
            transfer_id,
            message: message.into(),
        }
    }

    fn line(&self, zone: DisplayTimeZone) -> String {
        let level = if self.failed { "错误" } else { "信息" };
        format!("[{}] [{level}] {}", zone.format(self.time), self.message)
    }
}

enum Edit {
    Rename(Entry, bool),
    Move(Entry),
    Copy(Entry),
    Delete(Entry),
}

struct InlineRename {
    entry: Entry,
    tree: bool,
    input: Entity<InputState>,
    _subscription: Subscription,
    _blur: Subscription,
}

struct TreeCreate {
    prefix: String,
    directory: bool,
    input: Entity<InputState>,
    _subscription: Subscription,
    _blur: Subscription,
}

struct UsageScan {
    scan_objects: bool,
    abort: tokio::task::AbortHandle,
    _completion: Task<()>,
}

impl Drop for UsageScan {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransferDirection {
    Upload,
    Download,
    Clipboard,
    Copy,
    Move,
    Rename,
}

impl TransferDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Upload => "上传",
            Self::Download => "下载",
            Self::Clipboard => "剪贴板",
            Self::Copy => "复制",
            Self::Move => "移动",
            Self::Rename => "重命名",
        }
    }
}

// S3 directory prefixes are empty (bucket root) or end with '/'.
struct TransferScopes {
    connection: u64,
    folders: Vec<(String, String)>,
    paths: Vec<(String, String)>,
}

impl TransferScopes {
    fn contains_folder(&self, bucket: &str, folder: &str) -> bool {
        self.folders
            .iter()
            .any(|(name, prefix)| name == bucket && folder == prefix)
            || self.paths.iter().any(|(name, path)| {
                name == bucket && path.ends_with('/') && folder.starts_with(path)
            })
    }

    fn contains_entry(&self, bucket: &str, key: &str, recursive: bool) -> bool {
        self.contains_folder(bucket, parent_prefix(key))
            || (recursive
                && self
                    .folders
                    .iter()
                    .any(|(name, prefix)| name == bucket && prefix.starts_with(key)))
            || self.paths.iter().any(|(name, path)| {
                name == bucket
                    && (key == path
                        || (path.ends_with('/') && key.starts_with(path))
                        || (recursive && path.starts_with(key)))
            })
    }

    fn affects_listing(&self, bucket: &str, prefix: &str) -> bool {
        // A parent can gain or lose an implicit S3 folder when its last object changes.
        self.contains_folder(bucket, prefix)
            || self
                .folders
                .iter()
                .any(|(name, folder)| name == bucket && folder.starts_with(prefix))
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.connection == other.connection
            && (self
                .folders
                .iter()
                .any(|(bucket, folder)| other.contains_folder(bucket, folder))
                || self
                    .paths
                    .iter()
                    .any(|(bucket, key)| other.contains_entry(bucket, key, key.ends_with('/')))
                || other
                    .folders
                    .iter()
                    .any(|(bucket, folder)| self.contains_folder(bucket, folder))
                || other
                    .paths
                    .iter()
                    .any(|(bucket, key)| self.contains_entry(bucket, key, key.ends_with('/'))))
    }
}

struct TransferTask {
    saved: uuid::Uuid,
    direction: TransferDirection,
    started: Instant,
    destination: Option<PathBuf>,
    id: u64,
    target: String,
    status: String,
    running: bool,
    connection: u64,
    bucket: String,
    items: Vec<TransferItem>,
    expanded: bool,
    cancel: CancellationToken,
}

enum WorkerRow {
    Summary(usize, usize),
    Active(usize, String),
}

struct TransferItem {
    id: usize,
    directory: bool,
    progress: TransferProgress,
    state: Arc<Mutex<TransferState>>,
    cancel: CancellationToken,
}

#[derive(Clone)]
struct TransferProgress {
    key: String,
    size: u64,
    counter: Arc<AtomicU64>,
    sampled: u64,
    sample_time: Instant,
    speed: f64,
}

impl TransferProgress {
    fn sample(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.sample_time).as_secs_f64();
        let bytes = self.counter.load(Ordering::Relaxed).min(self.size);
        if elapsed > 0.0 {
            let rate = bytes.saturating_sub(self.sampled) as f64 / elapsed;
            let weight = 1.0 - (-elapsed / 0.8).exp();
            self.speed += (rate - self.speed) * weight;
        }
        self.sampled = bytes;
        self.sample_time = now;
    }
}

struct FilePreview {
    entry: Entry,
    content: PreviewContent,
}

enum PreviewContent {
    Loading(preview::PreviewDownload),
    Ready(preview::TextPreview),
    #[cfg(windows)]
    Image(Arc<RenderImage>),
    #[cfg(windows)]
    Pdf(preview::PdfPreview),
    Unavailable(&'static str),
    Failed(SharedString),
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsPage {
    Appearance,
    Credentials,
    Preview,
    Transfer,
    Storage,
    Trash,
    Startup,
    Sync,
    DateTime,
}
pub(crate) struct Browser {
    runtime: Arc<tokio::runtime::Runtime>,
    limit_connection_buckets: bool,
    selected_buckets: std::collections::BTreeSet<String>,
    fetched_bucket_names: Option<Vec<String>>,
    region: Entity<InputState>,
    endpoint: Entity<InputState>,
    profile: Entity<InputState>,
    access_key: Entity<InputState>,
    secret_key: Entity<InputState>,
    preview_limit_mb: u32,
    #[cfg(windows)]
    preview_cache_usage: Option<u64>,
    #[cfg(windows)]
    preview_cache_busy: bool,
    quotas: BucketQuotas,
    quota_input: Entity<InputState>,
    bucket_usage: Option<i64>,
    server_capacity: Option<crate::services::store::ServerCapacity>,
    quota_query_failed: bool,
    usage_revision: u64,
    usage_scan: Option<UsageScan>,
    startup: Result<crate::services::startup::Registration, SharedString>,
    sync: Entity<SyncService>,
    observed_sync_run: Option<uuid::Uuid>,
    show_sync_tasks: bool,
    show_transfer_history: bool,
    etag_check: Option<entry_menu::EtagCheck>,
    new_sync_automatic: bool,
    sync_direction: crate::services::sync::Direction,
    sync_conflict_policy: ConflictPolicy,
    sync_remote: Option<(String, String)>,
    sync_local: Option<PathBuf>,
    preview: Option<FilePreview>,
    show_preview: bool,
    preview_window: AnyWindowHandle,
    confirming_preview_leave: bool,
    store: Option<Store>,
    server: Option<Server>,
    buckets: Vec<Bucket>,
    available_buckets: Vec<Bucket>,
    connection_settings: ConnectionSettings,
    bucket_revision: u64,
    connection_revision: u64,
    prefix: String,
    listings: HashMap<String, Vec<Entry>>,
    expanded: HashSet<String>,
    parked_buckets: HashMap<String, connection::BucketTree>,
    tree: Vec<(usize, Entry)>,
    visible_rows: Vec<Entry>,
    hide_dot_entries: bool,
    selected: Option<String>,
    object_multi: HashSet<String>,
    object_anchor: Option<String>,
    list_focus: FocusHandle,
    tree_focus: FocusHandle,
    tree_selected: Option<String>,
    tree_multi: HashSet<String>,
    tree_range_anchor: Option<String>,
    tree_create: Option<TreeCreate>,
    rename: Option<InlineRename>,
    list_scroll: UniformListScrollHandle,
    file_sort: file_list::FileSort,
    file_column_widths: [Rems; 3],
    file_column_resize: Option<file_list::ColumnResize>,
    grid_scroll: UniformListScrollHandle,
    grid_columns: std::rc::Rc<std::cell::Cell<usize>>,
    grid_layout: bool,
    #[cfg(windows)]
    thumbnails: file_grid::Thumbnails,
    tree_scroll: UniformListScrollHandle,
    busy: Option<SharedString>,
    write_scopes: HashMap<u64, TransferScopes>,
    upload_scopes: HashMap<u64, TransferScopes>,
    pending_listing_refreshes: Vec<TransferScopes>,
    refreshing_listings: bool,
    listing_epoch: u64,
    loading_folders: HashSet<String>,
    refresh_requested: bool,
    bucket_request: u64,
    opening_bucket: Option<String>,
    refreshing_buckets: bool,
    fetching_connection_buckets: bool,
    sharing_link: bool,
    trash_jobs: HashSet<(u64, String)>,
    trash_reload_requested: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    notice_timeout: Option<Task<()>>,
    show_output: bool,
    show_output_info: bool,
    output_failures_only: bool,
    output_transfer: Option<u64>,
    output: VecDeque<LogEntry>,
    settings_page: Option<SettingsPage>,
    settings_focus: FocusHandle,
    profiles: ConnectionProfiles,
    selected_profile: Option<uuid::Uuid>,
    connected_profile: Option<uuid::Uuid>,
    profile_name: Entity<InputState>,
    preview_limit_input: Entity<InputState>,
    time_zone: DisplayTimeZone,
    time_zone_input: Entity<InputState>,
    show_tasks: bool,
    tasks_maximized: bool,
    trash: Option<Vec<TrashEntry>>,
    trash_retention: TrashRetention,
    trash_days_input: Entity<InputState>,
    next_trash_cleanup: Instant,
    tasks_focus: FocusHandle,
    transfers: Vec<TransferTask>,
    journal_connection: Option<String>,
    retrying_saved_transfers: bool,
    next_transfer_id: u64,
    transfer_slots: TransferSlots,
    _watch_transfer_slots: Task<()>,
    transfer_parallelism_input: Entity<InputState>,
    task_scroll: UniformListScrollHandle,
    expanded_task_scroll: UniformListScrollHandle,
    upload_refresh_pending: bool,
}

impl Browser {
    pub(crate) fn new(
        runtime: Arc<tokio::runtime::Runtime>,
        sync: Entity<SyncService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut error: Option<SharedString> = None;
        let profiles = ConnectionSettings::path()
            .and_then(|path| ConnectionProfiles::load(&path))
            .unwrap_or_else(|failure| {
                error = Some(format!("{failure:#}").into());
                ConnectionProfiles::default()
            });
        let active = profiles.active.and_then(|id| profiles.get(id));
        let settings = active.map(|profile| &profile.settings);
        let profile_name = cx.new(|cx| {
            InputState::new(window, cx).default_value(
                active
                    .map_or("默认连接", |profile| profile.name.as_str())
                    .to_owned(),
            )
        });
        let (access_key, secret_key) = match settings.map(ConnectionSettings::keys).transpose() {
            Ok(keys) => keys.unwrap_or_default(),
            Err(failure) => {
                error = Some(format!("{failure:#}").into());
                Default::default()
            }
        };
        let mut input =
            |hint: &'static str, variable: &str, get: fn(&ConnectionSettings) -> &str| {
                let value = settings.as_ref().map_or_else(
                    || std::env::var(variable).unwrap_or_default(),
                    |settings| get(settings).to_owned(),
                );
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(hint)
                        .default_value(value)
                })
            };
        let default_bucket = ConnectionSettings::new(
            std::env::var("S3FS_BUCKET").unwrap_or_default(),
            String::new(),
            String::new(),
            String::new(),
        );
        let selected_buckets: std::collections::BTreeSet<_> = settings
            .unwrap_or(&default_bucket)
            .bucket_names()
            .map(str::to_owned)
            .collect();
        let region = input(
            "留空时使用 AWS 配置",
            "AWS_REGION",
            ConnectionSettings::region,
        );
        let endpoint = input(
            "例如 https://s3.example.com",
            "S3FS_ENDPOINT",
            ConnectionSettings::endpoint,
        );
        let profile = input(
            "留空时使用默认凭据链",
            "AWS_PROFILE",
            ConnectionSettings::profile,
        );
        let access_key = cx.new(|cx| InputState::new(window, cx).default_value(access_key));
        let secret_key = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .default_value(secret_key)
        });
        let preview_limit_mb = PreviewSettings::load().unwrap_or_else(|failure| {
            if error.is_none() {
                error = Some(format!("{failure:#}").into());
            }
            DEFAULT_PREVIEW_LIMIT_MB
        });
        let transfer_limit = TransferSettings::load()
            .unwrap_or_else(|failure| {
                if error.is_none() {
                    error = Some(format!("{failure:#}").into());
                }
                TransferSettings::default()
            })
            .limit();
        let transfer_slots = sync.read(cx).transfer_slots();
        transfer_slots.set_limit(transfer_limit);
        let mut workers = transfer_slots.subscribe();
        let watch_transfer_slots = cx.spawn(async move |view, cx| {
            while workers.changed().await.is_ok() {
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let transfer_parallelism_input =
            cx.new(|cx| InputState::new(window, cx).default_value(transfer_limit.to_string()));
        let hide_dot_entries = DotEntrySettings::load().unwrap_or_else(|failure| {
            if error.is_none() {
                error = Some(format!("{failure:#}").into());
            }
            false
        });
        let time_zone = DisplayTimeZone::load().unwrap_or_else(|failure| {
            if error.is_none() {
                error = Some(format!("{failure:#}").into());
            }
            DisplayTimeZone::default()
        });
        let time_zone_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("跟随系统")
                .default_value(time_zone.name())
        });
        let preview_limit_input =
            cx.new(|cx| InputState::new(window, cx).default_value(preview_limit_mb.to_string()));
        for field in [&endpoint, &region, &profile, &access_key, &secret_key] {
            cx.subscribe(field, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) && this.fetched_bucket_names.take().is_some()
                {
                    cx.notify();
                }
            })
            .detach();
        }
        endpoint.read(cx).focus_handle(cx).focus(window, cx);
        if settings.is_some() && error.is_none() {
            cx.defer_in(window, |this, window, cx| this.connect(false, window, cx));
        }
        let observed_sync_run = sync.read(cx).running();
        cx.observe(&sync, |this, _, cx| this.sync_changed(cx))
            .detach();
        let trash_retention = TrashRetention::load();
        let trash_days_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("0 或留空")
                .default_value(
                    trash_retention
                        .as_ref()
                        .ok()
                        .and_then(TrashRetention::days)
                        .map_or_else(String::new, |days| days.to_string()),
                )
        });
        let executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            loop {
                executor.timer(Duration::from_secs(60)).await;
                if view
                    .update(cx, |this, cx| this.clean_expired_trash(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut output = VecDeque::from([LogEntry::new(None, false, "应用已启动")]);
        if let Some(error) = &error {
            output.push_front(LogEntry::new(None, true, error.clone()));
        }
        if let Err(error) = &trash_retention {
            output.push_front(LogEntry::new(
                None,
                true,
                format!("无法读取回收站设置：{error:#}"),
            ));
        }
        Self {
            runtime,
            limit_connection_buckets: !selected_buckets.is_empty(),
            selected_buckets,
            fetched_bucket_names: None,
            region,
            endpoint,
            profile,
            access_key,
            secret_key,
            preview_limit_mb,
            #[cfg(windows)]
            preview_cache_usage: None,
            #[cfg(windows)]
            preview_cache_busy: false,
            quotas: BucketQuotas::load().unwrap_or_default(),
            quota_input: cx.new(|cx| InputState::new(window, cx)),
            bucket_usage: None,
            server_capacity: None,
            quota_query_failed: false,
            usage_revision: 0,
            usage_scan: None,
            startup: crate::services::startup::registered()
                .map_err(|error| error.to_string().into()),
            sync,
            observed_sync_run,
            show_sync_tasks: false,
            show_transfer_history: false,
            etag_check: None,
            new_sync_automatic: false,
            sync_direction: crate::services::sync::Direction::Both,
            sync_conflict_policy: ConflictPolicy::default(),
            sync_remote: None,
            sync_local: None,
            preview: None,
            show_preview: false,
            preview_window: window.window_handle(),
            confirming_preview_leave: false,
            store: None,
            server: None,
            buckets: Vec::new(),
            available_buckets: Vec::new(),
            connection_settings: ConnectionSettings::default(),
            bucket_revision: 0,
            connection_revision: 0,
            prefix: String::new(),
            listings: HashMap::new(),
            expanded: HashSet::new(),
            parked_buckets: HashMap::new(),
            tree: Vec::new(),
            visible_rows: Vec::new(),
            hide_dot_entries,
            selected: None,
            object_multi: HashSet::new(),
            object_anchor: None,
            list_focus: cx.focus_handle().tab_stop(true),
            tree_focus: cx.focus_handle().tab_stop(true),
            tree_selected: None,
            tree_multi: HashSet::new(),
            tree_range_anchor: None,
            tree_create: None,
            rename: None,
            list_scroll: Default::default(),
            file_sort: Default::default(),
            file_column_widths: [rems(20.), rems(10.), rems(6.)],
            file_column_resize: None,
            grid_scroll: Default::default(),
            grid_columns: std::rc::Rc::new(std::cell::Cell::new(1)),
            grid_layout: crate::services::settings::GridLayout::load().unwrap_or_default(),
            #[cfg(windows)]
            thumbnails: Default::default(),
            tree_scroll: Default::default(),
            busy: None,
            write_scopes: HashMap::new(),
            upload_scopes: HashMap::new(),
            pending_listing_refreshes: Vec::new(),
            refreshing_listings: false,
            listing_epoch: 0,
            loading_folders: HashSet::new(),
            refresh_requested: false,
            bucket_request: 0,
            opening_bucket: None,
            refreshing_buckets: false,
            fetching_connection_buckets: false,
            sharing_link: false,
            trash_jobs: HashSet::new(),
            trash_reload_requested: false,
            error,
            notice: None,
            notice_timeout: None,
            show_output: false,
            show_output_info: false,
            output_failures_only: false,
            output_transfer: None,
            output,
            settings_page: None,
            settings_focus: cx.focus_handle(),
            selected_profile: profiles.active,
            connected_profile: None,
            profiles,
            profile_name,
            preview_limit_input,
            time_zone,
            time_zone_input,
            show_tasks: false,
            tasks_maximized: false,
            trash: None,
            trash_retention: trash_retention.unwrap_or_default(),
            trash_days_input,
            next_trash_cleanup: Instant::now(),
            tasks_focus: cx.focus_handle().tab_stop(true),
            transfers: Vec::new(),
            journal_connection: None,
            retrying_saved_transfers: false,
            next_transfer_id: 0,
            transfer_slots,
            _watch_transfer_slots: watch_transfer_slots,
            transfer_parallelism_input,
            task_scroll: Default::default(),
            expanded_task_scroll: Default::default(),
            upload_refresh_pending: false,
        }
    }

    fn run<T: Send + 'static>(
        &mut self,
        label: &'static str,
        detail: String,
        future: impl Future<Output = Result<T>> + Send + 'static,
        finish: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.listing_epoch += 1;
        self.busy = Some(label.into());
        let operation = format!("{label}，{detail}");
        self.push_output(None, false, operation.clone());
        let started = Instant::now();
        self.error = None;
        self.clear_notice();
        let job = self.runtime.spawn(future);
        cx.spawn(async move |view, cx| {
            let result = job
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let _ = view.update(cx, |this, cx| {
                this.busy = None;
                let failed = result.is_err();
                finish(this, result, cx);
                let failed = failed || this.error.is_some();
                let status = if failed { "失败" } else { "完成" };
                this.push_output(
                    None,
                    failed,
                    format!(
                        "{operation}，{status}，{:.2} 秒",
                        started.elapsed().as_secs_f64()
                    ),
                );
                this.refresh_after_upload(cx);
                this.refresh_directory_listings(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn run_background<T: Send + 'static>(
        &mut self,
        label: &'static str,
        detail: String,
        future: impl Future<Output = Result<T>> + Send + 'static,
        finish: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let operation = format!("{label}，{detail}");
        self.log(false, operation.clone());
        let started = Instant::now();
        let job = self.runtime.spawn(future);
        cx.spawn(async move |view, cx| {
            let result = job
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let _ = view.update(cx, |this, cx| {
                let failed = result.is_err();
                let status = if failed { "失败" } else { "完成" };
                this.log(
                    failed,
                    format!(
                        "{operation}，{status}，{:.2} 秒",
                        started.elapsed().as_secs_f64()
                    ),
                );
                finish(this, result, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn reserve_write(&mut self, scope: TransferScopes, cx: &mut Context<Self>) -> Option<u64> {
        if !self.write_scope_available(&scope) {
            self.show_write_locked(cx);
            return None;
        }
        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        self.write_scopes.insert(id, scope);
        Some(id)
    }

    fn finish_write(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(scope) = self
            .write_scopes
            .remove(&id)
            .or_else(|| self.upload_scopes.remove(&id))
            && scope.connection == self.connection_revision
        {
            self.listing_epoch += 1;
            self.pending_listing_refreshes.push(scope);
            self.refresh_directory_listings(cx);
        }
    }

    fn write_scope(&self, store: &Store, keys: impl IntoIterator<Item = String>) -> TransferScopes {
        let bucket = store.bucket();
        let paths: Vec<_> = keys
            .into_iter()
            .map(|key| (bucket.to_owned(), key))
            .collect();
        let folders = paths
            .iter()
            .map(|(_, key)| (bucket.to_owned(), parent_prefix(key).to_owned()))
            .collect();
        TransferScopes {
            connection: self.connection_revision,
            folders,
            paths,
        }
    }

    fn run_write<T: Send + 'static>(
        &mut self,
        scope: TransferScopes,
        label: &'static str,
        detail: String,
        future: impl Future<Output = Result<T>> + Send + 'static,
        finish: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        let connection = scope.connection;
        let bucket = self.store.as_ref().map(|store| store.bucket().to_owned());
        let Some(id) = self.reserve_write(scope, cx) else {
            return false;
        };
        let gate = self.sync.read(cx).gate();
        self.run_background(
            label,
            detail,
            async move {
                let _guard = gate.read().await;
                future.await
            },
            move |this, result, cx| {
                let on_bucket = this.connection_revision == connection
                    && this.store.as_ref().map(Store::bucket) == bucket.as_deref();
                if on_bucket {
                    this.refresh_usage(cx);
                    finish(this, result, cx);
                } else if let Err(error) = result {
                    this.show_error(error, cx);
                }
                this.finish_write(id, cx);
                if on_bucket && this.trash.is_some() {
                    if this.trash_busy() {
                        this.trash_reload_requested = true;
                    } else {
                        this.open_trash(cx);
                    }
                }
            },
            cx,
        );
        true
    }

    fn write_locked_in(&self, bucket: &str, key: &str, recursive: bool) -> bool {
        self.write_scopes.values().any(|scope| {
            scope.connection == self.connection_revision
                && scope.contains_entry(bucket, key, recursive)
        })
    }

    fn folder_write_locked_in(&self, bucket: &str, folder: &str) -> bool {
        self.write_scopes.values().any(|scope| {
            scope.connection == self.connection_revision && scope.contains_folder(bucket, folder)
        })
    }

    fn folder_write_locked(&self, folder: &str) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| self.folder_write_locked_in(store.bucket(), folder))
    }

    fn write_locked(&self, key: &str, recursive: bool) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| self.write_locked_in(store.bucket(), key, recursive))
    }

    fn entry_write_locked(&self, entry: &Entry) -> bool {
        self.write_locked(entry.key(), entry.directory())
    }

    fn show_write_locked(&mut self, cx: &mut Context<Self>) {
        self.show_notice("该目录还有操作正在进行，请稍后重试", cx);
    }

    fn drop_surface(&self, surface: Div, prefix: String, cx: &Context<Self>) -> Div {
        if self.busy.is_some() || self.folder_write_locked(&prefix) {
            return surface;
        }
        surface
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.upload(prefix.clone(), paths.paths().to_vec(), cx);
            }))
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = if self.server.is_none() || self.settings_page.is_some() {
            self.settings_workspace(cx)
        } else {
            self.file_workspace(cx).into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = &event.keystroke;
                if key.key == "escape"
                    && this.show_preview
                    && this.settings_page.is_none()
                    && !this.tasks_maximized
                    && !window.has_active_dialog(cx)
                    && !window.has_active_sheet(cx)
                    && this.preview.as_ref().is_some_and(|preview| {
                        matches!(preview.content, PreviewContent::Loading(_))
                    })
                {
                    cx.stop_propagation();
                    this.cancel_preview(window, cx);
                    return;
                }
                if !key.modifiers.control || key.modifiers.alt || key.modifiers.platform {
                    return;
                }
                if !key.modifiers.shift
                    && key.key.eq_ignore_ascii_case("s")
                    && this.show_preview
                    && this.settings_page.is_none()
                    && !this.tasks_maximized
                    && !window.has_active_dialog(cx)
                    && !window.has_active_sheet(cx)
                    && this.save_text_preview(window, cx)
                {
                    cx.stop_propagation();
                    return;
                }
                if !key.modifiers.shift && key.key == "`" && !window.has_active_dialog(cx) {
                    cx.stop_propagation();
                    if !event.is_held {
                        this.toggle_output(cx);
                    }
                    return;
                }
                let query = if key.modifiers.shift && key.key.eq_ignore_ascii_case("p") {
                    ">"
                } else if !key.modifiers.shift && key.key.eq_ignore_ascii_case("f") {
                    ""
                } else {
                    return;
                };
                if !window.has_active_dialog(cx) {
                    cx.stop_propagation();
                    this.open_search(query, window, cx);
                }
            }))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(div().flex_1().min_h_0().overflow_hidden().child(content))
            .when_some(self.error.clone(), |view, error| {
                let details = error.clone();
                view.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_color(theme.danger)
                        .child(Icon::new(IconName::CircleAlert).size_4())
                        .child(div().flex_1().min_w_0().truncate().child(error))
                        .child(
                            Button::new("error-details")
                                .small()
                                .ghost()
                                .label("详情")
                                .on_click(move |_, window, cx| {
                                    let details = details.clone();
                                    let editor = cx.new(|cx| {
                                        EditorState::new(window, cx)
                                            .soft_wrap(true)
                                            .line_number(false)
                                            .default_value(details.to_string())
                                    });
                                    let focus = editor.clone();
                                    window.open_dialog(cx, move |dialog, _, _| {
                                        let details = details.clone();
                                        dialog
                                            .title("操作未完成")
                                            .child(
                                                Editor::new(&editor)
                                                    .readonly(true)
                                                    .aria_label("完整错误信息")
                                                    .h(rems(16.)),
                                            )
                                            .footer(
                                                Button::new("copy-error-details")
                                                    .small()
                                                    .icon(IconName::Copy)
                                                    .label("复制全部")
                                                    .on_click(move |_, _, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                details.to_string(),
                                                            ),
                                                        );
                                                    }),
                                            )
                                    });
                                    window.defer(cx, move |window, cx| {
                                        focus.read(cx).focus_handle(cx).focus(window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("dismiss")
                                .small()
                                .ghost()
                                .icon(IconName::X)
                                .tooltip("关闭错误提示")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(self.status_bar(cx))
    }
}

fn size_label(bytes: i64) -> String {
    let mut size = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if size < 1024. || unit == "TiB" {
            return if unit == "B" {
                format!("{bytes} B")
            } else {
                format!("{size:.1} {unit}")
            };
        }
        size /= 1024.;
    }
    unreachable!()
}
