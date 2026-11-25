//! Windows output: ViGEm virtual Xbox 360 controller plus SendInput for
//! mouse and keyboard.

use crate::core::Sink;
use allypad_proto::PadState;
use std::mem::size_of;
use std::sync::atomic::{AtomicU8, Ordering};
use crate::core::MAX_PLAYERS;
use std::sync::Arc;
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

type Rumble = Arc<(AtomicU8, AtomicU8)>;

pub struct WinSink {
    client: Option<Arc<Client>>,
    targets: Vec<Option<Xbox360Wired<Arc<Client>>>>,
    rumble: Vec<Rumble>,
    pub pad_error: Option<String>,
}

impl WinSink {
    pub fn new() -> WinSink {
        let (client, pad_error) = match Client::connect() {
            Ok(client) => (Some(Arc::new(client)), None),
            Err(e) => (None, Some(format!("{e:?}"))),
        };
        WinSink {
            client,
            targets: (0..MAX_PLAYERS).map(|_| None).collect(),
            rumble: (0..MAX_PLAYERS).map(|_| Arc::new((AtomicU8::new(0), AtomicU8::new(0)))).collect(),
            pad_error,
        }
    }

    /// Look for the driver again (it may have been installed since start).
    pub fn retry_driver(&mut self) {
        if self.client.is_none() {
            if let Ok(c) = Client::connect() {
                self.client = Some(Arc::new(c));
                self.pad_error = None;
            }
        }
    }

    fn plug(&mut self, slot: usize) {
        let Some(client) = self.client.clone() else { return };
        let t = self.targets[slot].get_or_insert_with(|| Xbox360Wired::new(client, TargetId::XBOX360_WIRED));
        if t.is_attached() {
            return;
        }
        if let Err(e) = t.plugin() {
            self.pad_error = Some(format!("{e:?}"));
            return;
        }
        let _ = t.wait_ready();
        if let Ok(req) = t.request_notification() {
            let rumble = self.rumble[slot].clone();
            req.spawn_thread(move |_, n| {
                rumble.0.store(n.large_motor, Ordering::Relaxed);
                rumble.1.store(n.small_motor, Ordering::Relaxed);
            });
        }
    }
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, size_of::<INPUT>() as i32);
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn is_extended(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2C | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0x5F | 0x6F | 0x90 | 0xA3 | 0xA5 | 0xA6..=0xB7)
}

/// Volume, media, browser and sleep keys. Their scan codes are only
/// meaningful with the "extended" prefix; sent bare they are ordinary
/// letters (volume up would type "b"), so these go by key code alone.
fn is_media(vk: u16) -> bool {
    matches!(vk, 0x5F | 0xA6..=0xB7)
}

impl Sink for WinSink {
    fn pad(&mut self, slot: usize, p: &PadState) {
        self.plug(slot);
        if let Some(t) = self.targets[slot].as_mut() {
            if t.is_attached() {
                let g = XGamepad {
                    buttons: XButtons { raw: p.buttons },
                    left_trigger: p.lt,
                    right_trigger: p.rt,
                    thumb_lx: p.lx,
                    thumb_ly: p.ly,
                    thumb_rx: p.rx,
                    thumb_ry: p.ry,
                };
                let _ = t.update(&g);
            }
        }
    }

    fn pad_release(&mut self, slot: usize) {
        if let Some(t) = self.targets[slot].as_mut() {
            if t.is_attached() {
                let _ = t.unplug();
            }
        }
        self.rumble[slot].0.store(0, Ordering::Relaxed);
        self.rumble[slot].1.store(0, Ordering::Relaxed);
    }

    fn mouse_move(&mut self, dx: i32, dy: i32) {
        send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)]);
    }

    fn wheel(&mut self, v: i32, h: i32) {
        if v != 0 {
            send(&[mouse(0, 0, v, MOUSEEVENTF_WHEEL)]);
        }
        if h != 0 {
            send(&[mouse(0, 0, h, MOUSEEVENTF_HWHEEL)]);
        }
    }

    fn mouse_button(&mut self, bit: u8, down: bool) {
        let f = match (bit, down) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_RIGHTDOWN,
            (1, false) => MOUSEEVENTF_RIGHTUP,
            (2, true) => MOUSEEVENTF_MIDDLEDOWN,
            (_, _) => MOUSEEVENTF_MIDDLEUP,
        };
        send(&[mouse(0, 0, 0, f)]);
    }

    fn key(&mut self, vk: u16, down: bool) {
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        if is_extended(vk) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        // Send the key code and its scan code together: Windows and normal
        // programs act on the key code, games that read raw scan codes get
        // the scan code.
        let scan = if is_media(vk) { 0 } else { (unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) }) as u16 };
        let ki = KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 };
        send(&[INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } }]);
    }

    fn rumble(&mut self, slot: usize) -> (u8, u8) {
        (self.rumble[slot].0.load(Ordering::Relaxed), self.rumble[slot].1.load(Ordering::Relaxed))
    }

    fn pad_ok(&self) -> bool {
        self.client.is_some()
    }

    fn keep_awake(&mut self, on: bool) {
        // A virtual controller does not reset the idle timer, so hold the
        // display on while a session is active.
        unsafe {
            if on {
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
            } else {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }
}
