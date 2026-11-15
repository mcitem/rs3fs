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
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
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
const APP_ICON: u16 = 101;

#[derive(Clone, Copy)]
pub(crate) enum Event {
    Open,
    Exit,
}

thread_local! {
    static EVENTS: RefCell<Option<UnboundedSender<Event>>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
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
        uID: 1,
        ..Default::default()
    }
}

fn add_icon(window: HWND) {
    let mut icon = icon_data(window);
    icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    icon.uCallbackMessage = TRAY_CALLBACK;
    icon.hIcon = load_icon(APP_ICON, SM_CXSMICON, SM_CYSMICON);
    for (slot, ch) in icon.szTip.iter_mut().zip("rs3fs".encode_utf16()) {
        *slot = ch;
    }
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

fn window_hwnd(window: &gpui_kit::Window) -> Option<HWND> {
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

fn show_menu(window: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, OPEN_MENU as usize, w!("打开 rs3fs"));
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, EXIT_MENU as usize, w!("退出（停止同步）"));
        let mut point = windows_sys::Win32::Foundation::POINT::default();
        GetCursorPos(&mut point);
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
            WM_CONTEXTMENU | WM_RBUTTONUP => show_menu(window),
            _ => {}
        },
        WM_TIMER if wparam == RETRY_ICON => add_icon(window),
        message if message != 0 && message == TASKBAR_CREATED.get() => add_icon(window),
        // SAFETY: Windows supplies these parameters to the registered window callback.
        _ => return unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
    0
}
