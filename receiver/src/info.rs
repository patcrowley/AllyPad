//! What the laptop reports back to the Ally: volume, battery and the
//! program in the foreground. Collected on its own thread once a second.

use allypad_proto::Info;
use std::sync::{Arc, Mutex};

/// Foreground programs that are never "the game".
const BORING: [&str; 12] = [
    "explorer", "msedge", "searchhost", "shellexperiencehost", "startmenuexperiencehost", "applicationframehost",
    "lockapp", "taskmgr", "textinputhost", "systemsettings", "allypad-receiver", "dwm",
];

pub fn tidy_app(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or("");
    let stem = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file);
    if BORING.contains(&stem.to_ascii_lowercase().as_str()) {
        String::new()
    } else {
        stem.to_string()
    }
}

pub fn start() -> Arc<Mutex<Info>> {
    let shared = Arc::new(Mutex::new(Info { volume: 255, battery: 255, ..Info::default() }));
    let out = shared.clone();
    std::thread::spawn(move || platform::run(out));
    shared
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe fn volume(en: &IMMDeviceEnumerator) -> Option<(u8, bool)> {
        // Ask for the default device every time: it changes when headphones go in.
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None).ok()?;
        let level = vol.GetMasterVolumeLevelScalar().ok()?;
        let muted = vol.GetMute().ok()?.as_bool();
        Some(((level * 100.0).round().clamp(0.0, 100.0) as u8, muted))
    }

    unsafe fn battery() -> (u8, bool) {
        let mut st = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut st).is_ok() && st.BatteryLifePercent <= 100 && st.BatteryFlag & 128 == 0 {
            (st.BatteryLifePercent, st.ACLineStatus == 1)
        } else {
            (255, false)
        }
    }

    unsafe fn foreground() -> String {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if ok {
            tidy_app(&String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            String::new()
        }
    }

    pub fn run(out: Arc<Mutex<Info>>) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let en: Option<IMMDeviceEnumerator> = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok();
            loop {
                let (volume, muted) = en.as_ref().and_then(|e| volume(e)).unwrap_or((255, false));
                let (battery, charging) = battery();
                let app = foreground();
                *out.lock().unwrap() = Info { volume, muted, battery, charging, player: 0, app };
                std::thread::sleep(std::time::Duration::from_millis(700));
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub fn run(out: Arc<Mutex<Info>>) {
        // Sample values so the pipeline and the UI can be exercised.
        if std::env::var("ALLYPAD_SIM").is_ok() {
            *out.lock().unwrap() = Info {
                volume: 42,
                muted: false,
                battery: 76,
                charging: true,
                player: 0,
                app: tidy_app(r"C:\Games\Spider-Man2.exe"),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tidy_app;
    #[test]
    fn app_names() {
        assert_eq!(tidy_app(r"C:\Program Files\Game\Spider-Man2.exe"), "Spider-Man2");
        assert_eq!(tidy_app(r"C:\Windows\explorer.exe"), "");
        assert_eq!(tidy_app(r"C:\x\MSEdge.EXE"), "");
    }
}
