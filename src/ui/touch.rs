use gpui_kit::{
    App, GestureTuning, Pixels, PlatformInput, Point, TouchEvent, TouchId, TouchPhase, Window,
    point, px,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{collections::HashMap, mem::size_of};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::ScreenToClient,
    UI::{
        Input::{GetCurrentInputMessageSource, IMDT_TOUCH, INPUT_MESSAGE_SOURCE, Touch::*},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetMessageExtraInfo, WM_ACTIVATEAPP, WM_CANCELMODE, WM_CHAR, WM_IME_COMPOSITION,
            WM_IME_STARTCOMPOSITION, WM_MOUSEFIRST, WM_MOUSELAST, WM_NCDESTROY, WM_TOUCH,
        },
    },
};

#[derive(Clone, Copy)]
struct Contact {
    native_id: u32,
    phase: TouchPhase,
    position: Point<f32>,
}

enum TouchMessage {
    Contacts(Vec<Contact>),
    Cancel,
    TextInput,
}

type TouchSender = UnboundedSender<TouchMessage>;

struct ActiveContact {
    id: TouchId,
    start: Point<Pixels>,
    last: Point<Pixels>,
    stayed_within_slop: bool,
}

impl ActiveContact {
    fn move_to(&mut self, position: Point<Pixels>, slop: Pixels) {
        self.last = position;
        self.stayed_within_slop &= (position - self.start).magnitude() <= f64::from(slop);
    }

    fn release_position(&self, position: Point<Pixels>, slop: Pixels) -> Point<Pixels> {
        // GPUI hit-tests a tap at release. Use its starting position for
        // stationary contacts so finger jitter cannot miss a button edge.
        if self.stayed_within_slop && (position - self.start).magnitude() <= f64::from(slop) {
            self.start
        } else {
            position
        }
    }
}

pub(crate) fn attach(
    window: &mut Window,
    cx: &mut App,
    on_text_input: impl Fn(&mut Window, &mut App) + 'static,
) {
    const SUBCLASS_ID: usize = 1;

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as HWND;
    let (send, mut receive) = unbounded_channel();
    let state = Box::into_raw(Box::new(send));
    unsafe {
        if SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, state as usize) == 0 {
            drop(Box::from_raw(state));
            return;
        }
        RegisterTouchWindow(hwnd, 0);
    }

    // Dispatch on GPUI's foreground executor, outside the native callback.
    window
        .spawn(cx, async move |cx| {
            let mut active = HashMap::new();
            let mut next_id = 0;
            let slop = GestureTuning::default().touch_slop;
            while let Some(message) = receive.recv().await {
                let result = cx.update(|window, cx| match message {
                    TouchMessage::Contacts(contacts) => {
                        for contact in contacts {
                            let scale = window.scale_factor();
                            let mut position = point(
                                px(contact.position.x / scale),
                                px(contact.position.y / scale),
                            );
                            let id = match contact.phase {
                                TouchPhase::Started => {
                                    if active.contains_key(&contact.native_id) {
                                        continue;
                                    }
                                    next_id += 1;
                                    let id = TouchId(next_id);
                                    active.insert(
                                        contact.native_id,
                                        ActiveContact {
                                            id,
                                            start: position,
                                            last: position,
                                            stayed_within_slop: true,
                                        },
                                    );
                                    id
                                }
                                TouchPhase::Moved => {
                                    let Some(active) = active.get_mut(&contact.native_id) else {
                                        continue;
                                    };
                                    active.move_to(position, slop);
                                    active.id
                                }
                                TouchPhase::Ended | TouchPhase::Cancelled => {
                                    let Some(active) = active.remove(&contact.native_id) else {
                                        continue;
                                    };
                                    if contact.phase == TouchPhase::Ended {
                                        position = active.release_position(position, slop);
                                    }
                                    active.id
                                }
                            };
                            window.dispatch_event(
                                PlatformInput::Touch(TouchEvent {
                                    id,
                                    phase: contact.phase,
                                    position,
                                    ..Default::default()
                                }),
                                cx,
                            );
                        }
                    }
                    TouchMessage::Cancel => {
                        for (_, contact) in active.drain() {
                            window.dispatch_event(
                                PlatformInput::Touch(TouchEvent {
                                    id: contact.id,
                                    phase: TouchPhase::Cancelled,
                                    position: contact.last,
                                    ..Default::default()
                                }),
                                cx,
                            );
                        }
                    }
                    TouchMessage::TextInput => on_text_input(window, cx),
                });
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    state: usize,
) -> LRESULT {
    match message {
        WM_NCDESTROY => unsafe {
            RemoveWindowSubclass(hwnd, Some(window_proc), subclass_id);
            drop(Box::from_raw(state as *mut TouchSender));
        },
        // The IME and Unicode input can bypass GPUI's key-down events.
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION | WM_CHAR
            if message != WM_CHAR || (wparam >= 0x20 && wparam != 0x7f) =>
        {
            let send = unsafe { &*(state as *const TouchSender) };
            let _ = send.send(TouchMessage::TextInput);
        }
        WM_TOUCH => {
            let send = unsafe { &*(state as *const TouchSender) };
            let handle = lparam as HTOUCHINPUT;
            let count = (wparam & 0xffff) as u32;
            let mut inputs = vec![TOUCHINPUT::default(); count as usize];
            if unsafe {
                GetTouchInputInfo(
                    handle,
                    count,
                    inputs.as_mut_ptr(),
                    size_of::<TOUCHINPUT>() as i32,
                )
            } != 0
            {
                unsafe { CloseTouchInputHandle(handle) };
                let mut origin = POINT::default();
                if unsafe { ScreenToClient(hwnd, &mut origin) } != 0 {
                    let contacts = inputs
                        .into_iter()
                        .filter(|input| input.dwFlags & TOUCHEVENTF_PEN == 0)
                        .map(|input| Contact {
                            native_id: input.dwID,
                            phase: contact_phase(input.dwFlags),
                            position: point(
                                input.x as f32 / 100. + origin.x as f32,
                                input.y as f32 / 100. + origin.y as f32,
                            ),
                        })
                        .collect::<Vec<_>>();
                    if !contacts.is_empty() {
                        let _ = send.send(TouchMessage::Contacts(contacts));
                    }
                } else {
                    let _ = send.send(TouchMessage::Cancel);
                }
                return 0;
            }
        }
        WM_CANCELMODE | WM_ACTIVATEAPP if message == WM_CANCELMODE || wparam == 0 => {
            let send = unsafe { &*(state as *const TouchSender) };
            let _ = send.send(TouchMessage::Cancel);
        }
        WM_MOUSEFIRST..=WM_MOUSELAST if is_promoted_touch_mouse_message() => {
            return 0;
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn contact_phase(flags: u32) -> TouchPhase {
    if flags & TOUCHEVENTF_PALM != 0 {
        TouchPhase::Cancelled
    } else if flags & TOUCHEVENTF_DOWN != 0 {
        TouchPhase::Started
    } else if flags & TOUCHEVENTF_UP != 0 {
        TouchPhase::Ended
    } else {
        TouchPhase::Moved
    }
}

fn is_promoted_touch_mouse_message() -> bool {
    // A promoted mouse message can repeat an action already handled by GPUI's
    // touch recognizer. Check the device source even if the legacy tag is absent.
    let mut source = INPUT_MESSAGE_SOURCE::default();
    let device =
        (unsafe { GetCurrentInputMessageSource(&mut source) } != 0).then_some(source.deviceType);
    is_touch_mouse_message(device, unsafe { GetMessageExtraInfo() })
}

fn is_touch_mouse_message(device: Option<i32>, extra_info: LPARAM) -> bool {
    use windows_sys::Win32::UI::Input::{IMDT_PEN, IMDT_TOUCHPAD};

    match device {
        Some(IMDT_TOUCH) => true,
        Some(IMDT_PEN | IMDT_TOUCHPAD) => false,
        _ => extra_info as u32 & 0xffff_ff80 == 0xff51_5780,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promoted_touch_is_filtered_by_device_or_legacy_signature() {
        use windows_sys::Win32::UI::Input::{IMDT_MOUSE, IMDT_PEN, IMDT_TOUCHPAD};

        let touch_signature = 0xff51_5785u32 as LPARAM;
        assert!(is_touch_mouse_message(Some(IMDT_TOUCH), 0));
        assert!(is_touch_mouse_message(None, touch_signature));
        assert!(!is_touch_mouse_message(Some(IMDT_MOUSE), 0));
        assert!(!is_touch_mouse_message(Some(IMDT_PEN), touch_signature));
        assert!(!is_touch_mouse_message(
            Some(IMDT_TOUCHPAD),
            touch_signature
        ));
        assert!(!is_touch_mouse_message(None, 0xff51_5705u32 as LPARAM));
    }

    #[test]
    fn tap_targets_initial_contact_and_pan_keeps_actual_release() {
        let slop = GestureTuning::default().touch_slop;
        let start = point(px(100.), px(100.));
        let nearby = point(px(104.), px(101.));
        let mut contact = ActiveContact {
            id: TouchId(1),
            start,
            last: start,
            stayed_within_slop: true,
        };

        contact.move_to(nearby, slop);
        assert_eq!(contact.release_position(nearby, slop), start);

        contact.move_to(point(px(100.), px(125.)), slop);
        contact.move_to(nearby, slop);
        assert_eq!(contact.release_position(nearby, slop), nearby);
    }
}
