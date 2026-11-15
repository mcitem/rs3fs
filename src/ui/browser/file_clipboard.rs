use super::*;
use crate::services::transfer_journal::{TransferJournal, TransferOperation};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use windows_sys::Win32::{
    Foundation::{GlobalFree, HWND},
    System::{DataExchange::*, Memory::*},
    UI::Shell::{DROPFILES, DragQueryFileW},
};

const FILE_DROP: u32 = 15;

struct Clipboard {
    _thread_bound: std::marker::PhantomData<*mut ()>,
}

impl Clipboard {
    fn open(hwnd: HWND) -> Result<Self> {
        // SAFETY: The caller provides a live window handle or NULL. The guard
        // closes the clipboard on the same thread after a successful open.
        ensure!(
            unsafe { OpenClipboard(hwnd) } != 0,
            "无法打开剪贴板，请重试"
        );
        Ok(Self {
            _thread_bound: std::marker::PhantomData,
        })
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: This guard was created after OpenClipboard succeeded.
        unsafe { CloseClipboard() };
    }
}

fn read_files() -> Result<Vec<PathBuf>> {
    let _clipboard = Clipboard::open(std::ptr::null_mut())?;
    // SAFETY: The clipboard stays open while the borrowed CF_HDROP is in use.
    let data = unsafe { GetClipboardData(FILE_DROP) };
    if data.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: A NULL buffer requests the file count without writing to memory.
    let count = unsafe { DragQueryFileW(data, u32::MAX, std::ptr::null_mut(), 0) };
    (0..count)
        .map(|index| {
            // SAFETY: The index is within the reported count. A NULL buffer
            // requests the path length without writing to memory.
            let size = unsafe { DragQueryFileW(data, index, std::ptr::null_mut(), 0) };
            let capacity = size.checked_add(1).context("剪贴板文件路径过长")?;
            let mut buffer = vec![0u16; capacity as usize];
            // SAFETY: The buffer can hold exactly the capacity passed to Windows.
            let copied = unsafe { DragQueryFileW(data, index, buffer.as_mut_ptr(), capacity) };
            ensure!(copied == size && size != 0, "无法读取剪贴板文件路径");
            Ok(PathBuf::from(std::ffi::OsString::from_wide(
                &buffer[..size as usize],
            )))
        })
        .collect()
}

fn write_files(window: &Window, paths: &[PathBuf]) -> Result<()> {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        anyhow::bail!("无法取得 Windows 窗口");
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        anyhow::bail!("无法取得 Windows 窗口");
    };
    let mut files = Vec::new();
    for path in paths {
        files.extend(path.as_os_str().encode_wide());
        files.push(0);
    }
    files.push(0);
    let offset = std::mem::size_of::<DROPFILES>();
    let bytes = std::mem::size_of_val(files.as_slice())
        .checked_add(offset)
        .context("文件剪贴板数据过大")?;
    let _clipboard = Clipboard::open(handle.hwnd.get() as HWND)?;
    // SAFETY: The locked allocation holds the aligned header and UTF-16 data.
    // We unlock it before transferring it to the clipboard, and free it only
    // when the transfer fails or the allocation could not be locked.
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        ensure!(!memory.is_null(), "无法分配文件剪贴板内存");
        let data = GlobalLock(memory);
        if data.is_null() {
            GlobalFree(memory);
            anyhow::bail!("无法写入文件剪贴板");
        }
        data.cast::<DROPFILES>().write(DROPFILES {
            pFiles: offset as u32,
            pt: Default::default(),
            fNC: 0,
            fWide: 1,
        });
        std::ptr::copy_nonoverlapping(
            files.as_ptr(),
            data.cast::<u8>().add(offset).cast(),
            files.len(),
        );
        GlobalUnlock(memory);
        if EmptyClipboard() == 0 || SetClipboardData(FILE_DROP, memory).is_null() {
            GlobalFree(memory);
            anyhow::bail!("无法写入文件剪贴板");
        }
    }
    Ok(())
}

impl Browser {
    pub(super) fn paste_clipboard_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        match read_files() {
            Ok(paths) if paths.is_empty() => self.show_notice("剪贴板里没有文件", cx),
            Ok(paths) => self.upload(self.prefix.clone(), paths, cx),
            Err(error) => self.show_error(error, cx),
        }
    }

    pub(super) fn copy_objects_to_clipboard(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let entries: Vec<_> = self
            .rows()
            .iter()
            .filter(|entry| self.object_selected(entry.key()))
            .cloned()
            .collect();
        if !entries.is_empty() {
            self.queue_transfer(
                store.bucket().to_owned(),
                TransferOperation::Clipboard { entries },
                cx,
            );
        }
    }

    pub(super) fn publish_saved_clipboard(
        &mut self,
        task: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.journal_connection.as_deref() else {
            return;
        };
        let result = TransferJournal::clipboard_paths(connection, task).and_then(|paths| {
            write_files(window, &paths)?;
            Ok(paths.len())
        });
        match result {
            Ok(count) => self.show_notice(format!("已复制 {count} 项到文件剪贴板"), cx),
            Err(error) => self.show_error(error, cx),
        }
    }
}
