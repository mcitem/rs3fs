use std::time::{Duration, Instant};

use gpui_kit::Window;
use windows::Win32::{
    Foundation::HWND,
    System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    UI::Shell::{ITaskbarList3, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TaskbarList},
};

pub(super) const PROGRESS_SCALE: u64 = 1_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Progress {
    Hidden,
    Indeterminate,
    Determinate(u64),
}

#[derive(Default)]
pub(super) struct TaskbarProgress {
    taskbar: Option<ITaskbarList3>,
    displayed: Option<(Progress, Instant)>,
}

impl TaskbarProgress {
    pub(super) fn update(&mut self, window: &Window, progress: Progress) {
        if progress == Progress::Hidden && self.displayed.is_none() {
            return;
        }
        if self.displayed.is_some_and(|(previous, updated)| {
            previous == progress && updated.elapsed() < Duration::from_secs(2)
        }) {
            return;
        }
        let Some(hwnd) = super::tray::window_hwnd(window) else {
            return;
        };

        // GPUI initializes OLE on the window thread. Retry on a later tick if
        // Explorer is unavailable while starting up or recreating the button.
        let result = unsafe {
            let taskbar = match &self.taskbar {
                Some(taskbar) => taskbar,
                None => {
                    let Ok(taskbar) = CoCreateInstance::<_, ITaskbarList3>(
                        &TaskbarList,
                        None,
                        CLSCTX_INPROC_SERVER,
                    ) else {
                        return;
                    };
                    if taskbar.HrInit().is_err() {
                        return;
                    }
                    self.taskbar.insert(taskbar)
                }
            };
            let hwnd = HWND(hwnd);
            match progress {
                Progress::Hidden => taskbar.SetProgressState(hwnd, TBPF_NOPROGRESS),
                Progress::Indeterminate => taskbar.SetProgressState(hwnd, TBPF_INDETERMINATE),
                Progress::Determinate(value) => {
                    taskbar.SetProgressValue(hwnd, value, PROGRESS_SCALE)
                }
            }
        };
        if result.is_ok() {
            self.displayed = (progress != Progress::Hidden).then(|| (progress, Instant::now()));
        } else {
            self.taskbar = None;
        }
    }
}
