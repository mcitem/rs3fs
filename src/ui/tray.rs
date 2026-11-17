use anyhow::{Context, Result};
use std::{
    cell::{Cell, RefCell},
    fs::{File, OpenOptions, TryLockError},
    ptr::null_mut,
    time::Duration,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    w,
};

const WINDOW_CLASS: *const u16 = w!("S3FS.TrayWindow.1");
// Keep this hidden window name so either version can activate an existing instance.
const MESSAGE_WINDOW_TITLE: *const u16 = w!("s3fs");
const TRAY_CALLBACK: u32 = WM_APP + 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
const OPEN_WINDOW: u32 = WM_APP + 2;
const RETRY_ICON: usize = 1;
const OPEN_MENU: i32 = 1;
const EXIT_MENU: i32 = 2;
const PAUSE_SYNC_MENU: i32 = 3;
const TRAY_ICON_ID: u32 = 1;
const APP_ICON: u16 = 101;

#[derive(Clone, Copy)]
pub(crate) enum Event {
    Open,
    Exit,
    SetSyncPaused(bool),
}

thread_local! {
    static EVENTS: RefCell<Option<UnboundedSender<Event>>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static SYNC_PAUSED: Cell<bool> = const { Cell::new(false) };
    static TOOLTIP: RefCell<String> = RefCell::new("rs3fs".into());
}

pub(crate) struct Tray {
    window: HWND,
    _instance: File,
    events: UnboundedReceiver<Event>,
}

impl Tray {
    pub(crate) fn start(background: bool) -> Result<Option<Self>> {
        let config = crate::services::settings::ConnectionSettings::path()?;
        let directory = config.parent().context("找不到用户配置目录")?;
        std::fs::create_dir_all(directory)?;
        let instance = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("instance.lock"))?;
        match instance.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) if background => return Ok(None),
            Err(TryLockError::WouldBlock) => {
                // The first instance may still be starting and creating its message window.
                for _ in 0..50 {
                    unsafe {
                        let window = FindWindowW(WINDOW_CLASS, MESSAGE_WINDOW_TITLE);
                        if !window.is_null() {
                            let mut pid = 0;
                            GetWindowThreadProcessId(window, &mut pid);
                            AllowSetForegroundWindow(pid);
                            if PostMessageW(window, OPEN_WINDOW, 0, 0) != 0 {
                                return Ok(None);
                            }
                        }
                    }
                    match instance.try_lock() {
                        Ok(()) => return Self::create(instance).map(Some),
                        Err(TryLockError::WouldBlock) => {
                            std::thread::sleep(Duration::from_millis(50))
                        }
                        Err(TryLockError::Error(error)) => return Err(error.into()),
                    }
                }
                anyhow::bail!("rs3fs 已运行，但无法打开已有窗口。");
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        Self::create(instance).map(Some)
    }

    fn create(instance: File) -> Result<Self> {
        let (send, events) = unbounded_channel();
        EVENTS.replace(Some(send));
        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            TASKBAR_CREATED.set(taskbar_created);
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: module,
                lpszClassName: WINDOW_CLASS,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(std::io::Error::last_os_error()).context("无法注册托盘窗口");
            }
            // A hidden top-level window receives Explorer's TaskbarCreated broadcast.
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                WINDOW_CLASS,
                MESSAGE_WINDOW_TITLE,
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                module,
                std::ptr::null(),
            );
            if window.is_null() {
                return Err(std::io::Error::last_os_error()).context("无法创建托盘窗口");
            }
            add_icon(window);
            Ok(Self {
                window,
                _instance: instance,
                events,
            })
        }
    }

    pub(crate) async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub(crate) fn update_status(&self, paused: bool, sync: &str, transfers: Option<&str>) {
        SYNC_PAUSED.set(paused);
        let tooltip = if let Some(transfers) = transfers {
            format!("{}\n{}", shorten(sync, 61), shorten(transfers, 65))
        } else {
            shorten(sync, 127)
        };
        TOOLTIP.with_borrow_mut(|current| {
            if *current != tooltip {
                *current = tooltip;
                let mut icon = icon_data(self.window);
                icon.uFlags = NIF_TIP | NIF_SHOWTIP;
                fill_tip(&mut icon, current);
                unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon) };
            }
        });
    }
}

pub(crate) fn sync_status(service: &crate::services::sync_service::SyncService) -> String {
    if service.pause_error().is_some() {
        return "同步暂停状态读写失败".into();
    }
    if service.config_error().is_some() {
        return "同步配置读取失败".into();
    }
    let count = service.jobs().len();
    if count == 0 {
        return if service.is_paused() {
            "同步已暂停 · 无任务"
        } else {
            "暂无同步任务"
        }
        .into();
    }
    let active = service.running();
    let summary = if service.is_quitting() {
        "同步即将退出"
    } else if service.is_paused() {
        "同步已暂停"
    } else {
        "同步"
    };
    let mut jobs = service.jobs().iter().collect::<Vec<_>>();
    jobs.sort_by_key(|job| {
        (
            active != Some(job.id),
            service
                .conflicts(job.id)
                .is_none_or(|items| items.is_empty()),
            service.is_job_paused(job.id),
        )
    });
    let mut tip = format!("{summary} {count} 项");
    for job in jobs.into_iter().take(2) {
        let name = job
            .folder
            .file_name()
            .unwrap_or(job.folder.as_os_str())
            .to_string_lossy();
        let name = shorten(&name, 12);
        let status = service.task_status(job);
        tip.push_str(&format!("\n{name} · {status}"));
    }
    tip
}

pub(crate) fn shorten(text: &str, limit: usize) -> String {
    if text.encode_utf16().count() <= limit {
        return text.to_owned();
    }
    let mut remaining = limit;
    text.chars()
        .take_while(|ch| {
            let length = ch.len_utf16();
            if length > remaining {
                return false;
            }
            remaining -= length;
            true
        })
        .collect()
}

fn fill_tip(icon: &mut NOTIFYICONDATAW, tooltip: &str) {
    for (slot, ch) in icon.szTip.iter_mut().take(127).zip(tooltip.encode_utf16()) {
        *slot = ch;
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        EVENTS.replace(None);
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &icon_data(self.window));
            DestroyWindow(self.window);
        }
    }
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: TRAY_ICON_ID,
        ..Default::default()
    }
}

fn add_icon(window: HWND) {
    let mut icon = icon_data(window);
    icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    icon.uCallbackMessage = TRAY_CALLBACK;
    icon.hIcon = load_icon(APP_ICON, SM_CXSMICON, SM_CYSMICON);
    TOOLTIP.with_borrow(|tooltip| fill_tip(&mut icon, tooltip));
    unsafe {
        if Shell_NotifyIconW(NIM_ADD, &icon) == 0 {
            SetTimer(window, RETRY_ICON, 2000, None);
        } else {
            KillTimer(window, RETRY_ICON);
            icon.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &icon);
        }
    }
}

fn load_icon(
    resource_id: u16,
    width_metric: SYSTEM_METRICS_INDEX,
    height_metric: SYSTEM_METRICS_INDEX,
) -> HICON {
    unsafe {
        LoadImageW(
            GetModuleHandleW(null_mut()),
            resource_id as usize as _,
            IMAGE_ICON,
            GetSystemMetrics(width_metric),
            GetSystemMetrics(height_metric),
            LR_SHARED,
        ) as HICON
    }
}

pub(super) fn window_hwnd(window: &gpui_kit::Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
        _ => None,
    }
}

pub(crate) fn set_window_icon(window: &gpui_kit::Window) {
    if let Some(hwnd) = window_hwnd(window) {
        for (kind, width, height) in [
            (ICON_SMALL, SM_CXSMICON, SM_CYSMICON),
            (ICON_BIG, SM_CXICON, SM_CYICON),
        ] {
            let icon = load_icon(APP_ICON, width, height);
            if !icon.is_null() {
                unsafe { SendMessageW(hwnd, WM_SETICON, kind as WPARAM, icon as LPARAM) };
            }
        }
    }
}

pub(crate) fn set_window_visible(window: &gpui_kit::Window, visible: bool) {
    if let Some(hwnd) = window_hwnd(window) {
        unsafe {
            ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
    }
}

pub(crate) fn show_error(message: &str) {
    let message: Vec<_> = message.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(
            null_mut(),
            message.as_ptr(),
            w!("rs3fs"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn emit(event: Event) {
    EVENTS.with(|events| {
        if let Some(send) = events.borrow().as_ref() {
            let _ = send.send(event);
        }
    });
}

fn tray_menu_anchor(window: HWND, notification: u32, wparam: WPARAM, lparam: LPARAM) -> POINT {
    // Version 4 includes signed screen coordinates for mouse notifications.
    // WM_CONTEXTMENU does not guarantee an anchor in the tray callback.
    if notification == WM_RBUTTONUP && (lparam as u32 >> 16) == TRAY_ICON_ID {
        return POINT {
            x: wparam as i16 as i32,
            y: (wparam >> 16) as i16 as i32,
        };
    }

    let icon = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: window,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let mut bounds = RECT::default();
    if unsafe { Shell_NotifyIconGetRect(&icon, &mut bounds) } == 0 {
        return POINT {
            x: bounds.left + (bounds.right - bounds.left) / 2,
            y: bounds.top + (bounds.bottom - bounds.top) / 2,
        };
    }

    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) };
    point
}

fn show_menu(window: HWND, point: POINT) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, OPEN_MENU as usize, w!("打开 rs3fs"));
        let paused = SYNC_PAUSED.get();
        AppendMenuW(
            menu,
            MF_STRING,
            PAUSE_SYNC_MENU as usize,
            if paused {
                w!("恢复全部同步")
            } else {
                w!("暂停全部同步")
            },
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, EXIT_MENU as usize, w!("退出"));
        SetForegroundWindow(window);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            window,
            std::ptr::null(),
        );
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match command {
            OPEN_MENU => emit(Event::Open),
            EXIT_MENU => emit(Event::Exit),
            PAUSE_SYNC_MENU => emit(Event::SetSyncPaused(!paused)),
            _ => {}
        }
    }
}

// SAFETY: Windows must call this with the parameters of a dispatched message.
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        OPEN_WINDOW => emit(Event::Open),
        TRAY_CALLBACK => match lparam as u32 & 0xffff {
            NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP => emit(Event::Open),
            notification @ (WM_CONTEXTMENU | WM_RBUTTONUP) => {
                show_menu(
                    window,
                    tray_menu_anchor(window, notification, wparam, lparam),
                );
            }
            _ => {}
        },
        WM_TIMER if wparam == RETRY_ICON => add_icon(window),
        message if message != 0 && message == TASKBAR_CREATED.get() => add_icon(window),
        // SAFETY: Windows supplies these parameters to the registered window callback.
        _ => return unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
    0
}
