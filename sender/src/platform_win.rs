//! Windows hardware access: the Ally's built-in controller through XInput
//! and its motion sensors through Windows.Devices.Sensors.

use crate::process::RawPad;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::core::{w, PCSTR};
use windows::Devices::Sensors::{
    Accelerometer, AccelerometerReadingChangedEventArgs, Gyrometer, GyrometerReadingChangedEventArgs,
};
use windows::Foundation::TypedEventHandler;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XGamepad {
    buttons: u16,
    lt: u8,
    rt: u8,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XState {
    packet: u32,
    pad: XGamepad,
    // XInputGetStateEx writes a few extra bytes past the public struct.
    _reserved: [u32; 2],
}
#[repr(C)]
struct XVibration {
    left: u16,
    right: u16,
}
type GetState = unsafe extern "system" fn(u32, *mut XState) -> u32;
type SetState = unsafe extern "system" fn(u32, *mut XVibration) -> u32;

pub struct Pad {
    get: Option<GetState>,
    set: Option<SetState>,
    index: Option<u32>,
    last_scan: Option<Instant>,
    last_rumble: (u16, u16),
    last_good: Option<(Instant, RawPad)>,
    lost: bool,
}

fn axis(v: i16) -> f32 {
    (v as f32 / 32767.0).clamp(-1.0, 1.0)
}

impl Pad {
    pub fn new() -> Pad {
        let mut pad = Pad { get: None, set: None, index: None, last_scan: None, last_rumble: (0, 0), last_good: None, lost: true };
        unsafe {
            for name in [w!("xinput1_4.dll"), w!("xinput1_3.dll"), w!("xinput9_1_0.dll")] {
                let Ok(lib) = LoadLibraryW(name) else { continue };
                // Ordinal 100 is the variant that also reports the Xbox/Guide button.
                let get = GetProcAddress(lib, PCSTR(100usize as *const u8))
                    .or_else(|| GetProcAddress(lib, PCSTR(b"XInputGetState\0".as_ptr())));
                let set = GetProcAddress(lib, PCSTR(b"XInputSetState\0".as_ptr()));
                if let Some(g) = get {
                    pad.get = Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, GetState>(g));
                    pad.set = set.map(|s| std::mem::transmute::<unsafe extern "system" fn() -> isize, SetState>(s));
                    break;
                }
            }
        }
        pad
    }

    fn query(&self, i: u32) -> Option<XGamepad> {
        let get = self.get?;
        let mut st = XState::default();
        if unsafe { get(i, &mut st) } == 0 {
            Some(st.pad)
        } else {
            None
        }
    }

    pub fn read(&mut self, back_keys: &[u16]) -> RawPad {
        let mut raw = self.read_pad();
        // The back buttons are not part of the controller Windows sees.
        // Armoury Crate can make them send keyboard keys, which we watch for.
        for (i, vk) in back_keys.iter().take(2).enumerate() {
            if *vk != 0 && unsafe { GetAsyncKeyState(*vk as i32) } as u16 & 0x8000 != 0 {
                raw.extra |= 1 << i;
            }
        }
        raw
    }

    fn read_pad(&mut self) -> RawPad {
        let mut got = self.index.and_then(|i| self.query(i));
        if got.is_none() {
            self.index = None;
            // Probing empty slots is slow, so only rescan once a second.
            let due = self.last_scan.map_or(true, |t| t.elapsed().as_millis() > 1000);
            if due {
                self.last_scan = Some(Instant::now());
                for i in 0..4 {
                    if let Some(g) = self.query(i) {
                        self.index = Some(i);
                        got = Some(g);
                        break;
                    }
                }
            }
        }
        match got {
            Some(g) => {
                let raw = RawPad {
                    connected: true,
                    buttons: g.buttons,
                    lt: g.lt as f32 / 255.0,
                    rt: g.rt as f32 / 255.0,
                    lx: axis(g.lx),
                    ly: axis(g.ly),
                    rx: axis(g.rx),
                    ry: axis(g.ry),
                    extra: 0,
                };
                if self.lost {
                    self.lost = false;
                    crate::log(&format!("controller found in slot {}", self.index.unwrap_or(9)));
                }
                self.last_good = Some((Instant::now(), raw));
                raw
            }
            None => {
                // A failed read is not a release: keep what was held for a
                // moment, so a hiccup in the driver does not let go of
                // buttons and triggers mid-game.
                match self.last_good {
                    Some((t, raw)) if t.elapsed().as_millis() < 300 => raw,
                    _ => {
                        if !self.lost {
                            self.lost = true;
                            crate::log("controller read failed for 300 ms, treating it as disconnected");
                        }
                        RawPad::default()
                    }
                }
            }
        }
    }

    pub fn rumble(&mut self, large: f32, small: f32) {
        let v = ((large.clamp(0.0, 1.0) * 65535.0) as u16, (small.clamp(0.0, 1.0) * 65535.0) as u16);
        if v == self.last_rumble {
            return;
        }
        self.last_rumble = v;
        if let (Some(set), Some(i)) = (self.set, self.index) {
            let mut vib = XVibration { left: v.0, right: v.1 };
            unsafe {
                set(i, &mut vib);
            }
        }
    }
}

#[derive(Default)]
struct Inner {
    gyro: [f32; 3],
    accel: [f32; 3],
    dangle: [f32; 3],
    dangle_dt: f32,
    last_gyro: Option<Instant>,
}

#[derive(Clone, Copy, Default)]
pub struct MotionRaw {
    pub have_gyro: bool,
    pub have_accel: bool,
    pub gyro: [f32; 3],
    pub accel: [f32; 3],
    pub dangle: [f32; 3],
    pub dangle_dt: f32,
}

pub struct Sensors {
    inner: Arc<Mutex<Inner>>,
    have_gyro: Arc<AtomicBool>,
    have_accel: Arc<AtomicBool>,
}

impl Sensors {
    pub fn start() -> Sensors {
        let s = Sensors {
            inner: Arc::new(Mutex::new(Inner::default())),
            have_gyro: Arc::new(AtomicBool::new(false)),
            have_accel: Arc::new(AtomicBool::new(false)),
        };
        let (inner, hg, ha) = (s.inner.clone(), s.have_gyro.clone(), s.have_accel.clone());
        std::thread::spawn(move || {
            unsafe {
                let _ = RoInitialize(RO_INIT_MULTITHREADED);
            }
            // The sensor objects must stay alive for events to keep arriving.
            let mut keep_gyro = None;
            let mut keep_accel = None;
            match Gyrometer::GetDefault() {
                Ok(g) => {
                    let min = g.MinimumReportInterval().unwrap_or(16);
                    let _ = g.SetReportInterval(min.max(4));
                    let inner = inner.clone();
                    let h = TypedEventHandler::<Gyrometer, GyrometerReadingChangedEventArgs>::new(move |_, args| {
                        if let Some(a) = args.as_ref() {
                            let r = a.Reading()?;
                            let w = [r.AngularVelocityX()? as f32, r.AngularVelocityY()? as f32, r.AngularVelocityZ()? as f32];
                            let now = Instant::now();
                            let mut i = inner.lock().unwrap();
                            if let Some(t) = i.last_gyro {
                                let dt = now.duration_since(t).as_secs_f32().min(0.05);
                                for k in 0..3 {
                                    i.dangle[k] += w[k] * dt;
                                }
                                i.dangle_dt += dt;
                            }
                            i.last_gyro = Some(now);
                            i.gyro = w;
                        }
                        Ok(())
                    });
                    if g.ReadingChanged(&h).is_ok() {
                        hg.store(true, Ordering::Relaxed);
                        crate::log(&format!("gyro ready, report interval {} ms", g.ReportInterval().unwrap_or(0)));
                    }
                    keep_gyro = Some(g);
                }
                Err(e) => crate::log(&format!("no gyrometer: {e:?}")),
            }
            match Accelerometer::GetDefault() {
                Ok(a) => {
                    let min = a.MinimumReportInterval().unwrap_or(16);
                    let _ = a.SetReportInterval(min.max(8));
                    let inner = inner.clone();
                    let h = TypedEventHandler::<Accelerometer, AccelerometerReadingChangedEventArgs>::new(move |_, args| {
                        if let Some(ev) = args.as_ref() {
                            let r = ev.Reading()?;
                            let v = [r.AccelerationX()? as f32, r.AccelerationY()? as f32, r.AccelerationZ()? as f32];
                            inner.lock().unwrap().accel = v;
                        }
                        Ok(())
                    });
                    if a.ReadingChanged(&h).is_ok() {
                        ha.store(true, Ordering::Relaxed);
                        crate::log("accelerometer ready");
                    }
                    keep_accel = Some(a);
                }
                Err(e) => crate::log(&format!("no accelerometer: {e:?}")),
            }
            loop {
                std::thread::park();
                let _ = (&keep_gyro, &keep_accel);
            }
        });
        s
    }

    /// Latest readings; the integrated rotation is handed over and reset.
    pub fn take(&self) -> MotionRaw {
        let mut i = self.inner.lock().unwrap();
        let out = MotionRaw {
            have_gyro: self.have_gyro.load(Ordering::Relaxed),
            have_accel: self.have_accel.load(Ordering::Relaxed),
            gyro: i.gyro,
            accel: i.accel,
            dangle: i.dangle,
            dangle_dt: i.dangle_dt,
        };
        // A sensor that stopped reporting should read as still, not as its last value.
        if i.last_gyro.map_or(false, |t| t.elapsed().as_millis() > 250) {
            i.gyro = [0.0; 3];
        }
        i.dangle = [0.0; 3];
        i.dangle_dt = 0.0;
        out
    }
}

pub fn init() {
    use windows::Win32::System::Threading::*;
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
        // This process has no window of its own, so Windows 11 would treat
        // it as background work: slower cores and coarse timers. Opt out.
        let me = GetCurrentProcess();
        let _ = SetPriorityClass(me, HIGH_PRIORITY_CLASS);
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let _ = SetProcessInformation(
            me,
            ProcessPowerThrottling,
            &state as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

/// Keep the Ally's screen on while the app is running. Per-thread on Windows.
pub fn keep_awake() {
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
    }
}

/// Opens the touch UI. Prefers an Edge "app" window (no tabs or address
/// bar) in AllyPad's own Edge profile, started directly so the window can
/// be closed again when AllyPad stops. Returns that process when it worked.
pub fn open_ui(url: &str) -> Option<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut candidates = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            candidates.push(std::path::PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
    }
    if let Some(edge) = candidates.into_iter().find(|p| p.exists()) {
        let profile = crate::config::dir().join("edge-profile");
        let _ = std::fs::create_dir_all(&profile);
        let child = Command::new(edge)
            .arg(format!("--app={url}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--start-maximized",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-mode",
                "--hide-crash-restore-bubble",
            ])
            .spawn();
        match child {
            Ok(c) => return Some(c),
            Err(e) => crate::log(&format!("could not start Edge directly: {e}")),
        }
    }
    // Fallbacks: Edge through the shell, then whatever the default browser is.
    let app = Command::new("cmd")
        .args(["/C", "start", "", "msedge", &format!("--app={url}"), "--start-maximized"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !app {
        let _ = Command::new("cmd").args(["/C", "start", "", url]).creation_flags(CREATE_NO_WINDOW).status();
    }
    None
}

/// This device's battery: percent and whether it is on mains power.
pub fn battery() -> Option<(u8, bool)> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut st = SYSTEM_POWER_STATUS::default();
    unsafe {
        if GetSystemPowerStatus(&mut st).is_ok() && st.BatteryLifePercent <= 100 && st.BatteryFlag & 128 == 0 {
            return Some((st.BatteryLifePercent, st.ACLineStatus == 1));
        }
    }
    None
}

/// Windows 11's power mode ("Best power efficiency" / "Balanced" / "Best
/// performance"). There is no documented API for it; these two functions in
/// powrprof.dll are what Settings itself uses. Missing functions are simply
/// skipped.
mod power_mode {
    use std::sync::Mutex;
    use windows::core::{w, GUID, PCSTR};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    const EFFICIENCY: GUID = GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a);
    type Get = unsafe extern "system" fn(*mut GUID) -> u32;
    type Set = unsafe extern "system" fn(*const GUID) -> u32;
    static PREVIOUS: Mutex<Option<GUID>> = Mutex::new(None);

    unsafe fn funcs() -> Option<(Get, Set)> {
        let lib = LoadLibraryW(w!("powrprof.dll")).ok()?;
        let get = GetProcAddress(lib, PCSTR(b"PowerGetActualOverlayScheme\0".as_ptr()))
            .or_else(|| GetProcAddress(lib, PCSTR(b"PowerGetEffectiveOverlayScheme\0".as_ptr())))?;
        let set = GetProcAddress(lib, PCSTR(b"PowerSetActiveOverlayScheme\0".as_ptr()))?;
        Some((
            std::mem::transmute::<unsafe extern "system" fn() -> isize, Get>(get),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, Set>(set),
        ))
    }

    pub fn set(low: bool) {
        unsafe {
            let Some((get, set)) = funcs() else {
                crate::log("power mode: not available on this Windows version");
                return;
            };
            let mut prev = PREVIOUS.lock().unwrap();
            if low {
                if prev.is_none() {
                    let mut cur = GUID::zeroed();
                    if get(&mut cur) == 0 {
                        *prev = Some(cur);
                    }
                }
                let r = set(&EFFICIENCY);
                crate::log(&format!("power mode: best power efficiency (result {r})"));
            } else if let Some(p) = prev.take() {
                let r = set(&p);
                crate::log(&format!("power mode: restored (result {r})"));
            }
        }
    }
}

/// Switch Windows to its most efficient power mode, or back to what it was.
pub fn low_power(on: bool) {
    power_mode::set(on);
}

/// Dim the built-in screen (while touch is locked), or restore it. Runs in
/// the background because it goes through PowerShell.
pub fn dim(on: bool) {
    use std::os::windows::process::CommandExt;
    use std::sync::Mutex;
    static PREVIOUS: Mutex<Option<u8>> = Mutex::new(None);
    std::thread::spawn(move || {
        let ps = |script: &str| {
            std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", script])
                .creation_flags(0x0800_0000)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        let set = |level: u8| {
            ps(&format!(
                "Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightnessMethods | Invoke-CimMethod -MethodName WmiSetBrightness -Arguments @{{Timeout=0; Brightness={level}}} | Out-Null"
            ));
        };
        let mut prev = PREVIOUS.lock().unwrap();
        if on {
            if prev.is_none() {
                let cur = ps("(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness | Select-Object -First 1).CurrentBrightness")
                    .and_then(|s| s.parse::<u8>().ok());
                match cur {
                    Some(c) => *prev = Some(c),
                    None => {
                        crate::log("screen brightness could not be read; not dimming");
                        return;
                    }
                }
            }
            set(5);
        } else if let Some(p) = prev.take() {
            set(p);
        }
    });
}
