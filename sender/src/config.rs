//! Settings: connection, look, and the per-game-type profiles.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Stick {
    /// Inner dead zone, 0..0.5 of full travel.
    pub dz: f32,
    /// Travel at which output reaches 100 %.
    pub outer: f32,
    /// Response exponent: 1 linear, >1 finer control near centre.
    pub curve: f32,
    pub sens: f32,
    /// Minimum output once moving; cancels a game's own dead zone.
    pub anti: f32,
    pub invert_y: bool,
}
impl Default for Stick {
    fn default() -> Self {
        Stick { dz: 0.08, outer: 0.97, curve: 1.0, sens: 1.0, anti: 0.0, invert_y: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Trigger {
    pub start: f32,
    pub end: f32,
    /// Hair trigger: fully pressed as soon as it passes `start`.
    pub digital: bool,
}
impl Default for Trigger {
    fn default() -> Self {
        Trigger { start: 0.04, end: 0.96, digital: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MotionCfg {
    /// off | aim_stick | aim_mouse | steer | tilt
    pub mode: String,
    /// always | lt | rt | lb | rb
    pub when: String,
    pub sens: f32,
    /// Vertical speed relative to horizontal (aiming).
    pub vratio: f32,
    pub invert_x: bool,
    pub invert_y: bool,
    /// 0..1
    pub smooth: f32,
    /// Degrees per second (aiming) or degrees (tilt) ignored around rest.
    pub deadzone: f32,
    /// world | yaw | roll — which turn of the device aims left/right.
    pub axis: String,
    /// Degrees of tilt for full stick deflection.
    pub range: f32,
    /// Minimum stick push for gyro-to-stick aiming.
    pub floor: f32,
}
impl Default for MotionCfg {
    fn default() -> Self {
        MotionCfg {
            mode: "off".into(),
            when: "always".into(),
            sens: 1.0,
            vratio: 0.7,
            invert_x: false,
            invert_y: false,
            smooth: 0.25,
            deadzone: 1.0,
            axis: "world".into(),
            range: 35.0,
            floor: 0.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TrackpadCfg {
    /// mouse | stick | off
    pub mode: String,
    pub sens: f32,
    pub accel: bool,
    pub tap: bool,
    pub natural: bool,
}
impl Default for TrackpadCfg {
    fn default() -> Self {
        TrackpadCfg { mode: "mouse".into(), sens: 1.0, accel: true, tap: true, natural: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub blurb: String,
    /// pad | desktop
    pub output: String,
    pub ls: Stick,
    pub rs: Stick,
    pub lt: Trigger,
    pub rt: Trigger,
    pub swap_sticks: bool,
    pub swap_ab: bool,
    pub ls_dpad: bool,
    pub motion: MotionCfg,
    pub trackpad: TrackpadCfg,
    /// Six touch keys: "key:<vk>[+<vk>...]", "pad:<bits>", "mouse:<bits>" or "none".
    pub keys: Vec<String>,
    /// Per-button behaviour: button name (a b x y lb rb ls rs lt rt) to
    /// "turbo" or "toggle". Buttons not listed behave normally.
    pub modes: BTreeMap<String, String>,
    /// What the two back buttons do, same format as `keys`.
    pub back: Vec<String>,
}
impl Default for Profile {
    fn default() -> Self {
        Profile {
            id: "standard".into(),
            name: "Standard".into(),
            blurb: "Plain Xbox controller. Nothing altered.".into(),
            output: "pad".into(),
            ls: Stick::default(),
            rs: Stick::default(),
            lt: Trigger::default(),
            rt: Trigger::default(),
            swap_sticks: false,
            swap_ab: false,
            ls_dpad: false,
            motion: MotionCfg::default(),
            trackpad: TrackpadCfg::default(),
            keys: ["key:27", "key:9", "key:13", "key:32", "pad:1024", "key:77"].iter().map(|s| s.to_string()).collect(),
            modes: BTreeMap::new(),
            back: vec!["none".into(), "none".into()],
        }
    }
}

pub fn default_profiles() -> Vec<Profile> {
    let base = Profile::default();
    let keys = |k: [&str; 6]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        base.clone(),
        Profile {
            id: "shooter".into(),
            name: "Shooter".into(),
            blurb: "Hold the left trigger and aim by moving the Ally. Fine right-stick control, hair triggers.".into(),
            rs: Stick { dz: 0.06, curve: 1.6, ..Stick::default() },
            lt: Trigger { start: 0.08, end: 0.5, digital: false },
            rt: Trigger { start: 0.08, end: 0.5, digital: true },
            motion: MotionCfg { mode: "aim_stick".into(), when: "lt".into(), sens: 1.0, floor: 0.12, ..MotionCfg::default() },
            keys: keys(["key:27", "key:9", "key:77", "key:71", "pad:1024", "key:116"]),
            ..base.clone()
        },
        Profile {
            id: "racing".into(),
            name: "Racing".into(),
            blurb: "Tilt the Ally like a wheel to steer. Full-travel triggers for throttle and brake.".into(),
            ls: Stick { dz: 0.04, curve: 1.3, ..Stick::default() },
            lt: Trigger { start: 0.02, end: 0.98, digital: false },
            rt: Trigger { start: 0.02, end: 0.98, digital: false },
            motion: MotionCfg { mode: "steer".into(), smooth: 0.35, deadzone: 1.5, range: 35.0, ..MotionCfg::default() },
            keys: keys(["key:27", "key:13", "key:67", "key:82", "pad:1024", "key:77"]),
            ..base.clone()
        },
        Profile {
            id: "flight".into(),
            name: "Flight".into(),
            blurb: "Tilt to pitch and roll. Gentle sticks for small corrections.".into(),
            ls: Stick { dz: 0.05, curve: 1.5, ..Stick::default() },
            rs: Stick { dz: 0.05, curve: 1.5, ..Stick::default() },
            motion: MotionCfg { mode: "tilt".into(), smooth: 0.4, deadzone: 2.0, range: 30.0, ..MotionCfg::default() },
            keys: keys(["key:27", "key:71", "key:70", "key:66", "pad:1024", "key:77"]),
            ..base.clone()
        },
        Profile {
            id: "fighting".into(),
            name: "Fighting and retro".into(),
            blurb: "Left stick works as a D-pad, triggers are on/off, wide dead zones against stray inputs.".into(),
            ls: Stick { dz: 0.2, ..Stick::default() },
            rs: Stick { dz: 0.15, ..Stick::default() },
            lt: Trigger { start: 0.12, end: 0.5, digital: true },
            rt: Trigger { start: 0.12, end: 0.5, digital: true },
            ls_dpad: true,
            trackpad: TrackpadCfg { mode: "off".into(), ..TrackpadCfg::default() },
            keys: keys(["key:27", "key:13", "key:112", "key:113", "pad:1024", "key:9"]),
            ..base.clone()
        },
        Profile {
            id: "desktop".into(),
            name: "Desktop".into(),
            blurb: "Run the laptop from the couch: left stick moves the pointer, right stick scrolls, A and B click.".into(),
            output: "desktop".into(),
            ls: Stick { dz: 0.1, curve: 1.8, ..Stick::default() },
            rs: Stick { dz: 0.12, curve: 1.4, ..Stick::default() },
            keys: keys(["key:27", "key:91", "key:9", "key:179", "key:174", "key:175"]),
            ..base.clone()
        },
        Profile { id: "custom1".into(), name: "Custom 1".into(), blurb: "Yours to set up.".into(), ..base.clone() },
        Profile { id: "custom2".into(), name: "Custom 2".into(), blurb: "Yours to set up.".into(), ..base },
    ]
}

/// A computer this Ally has connected to before.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Saved {
    pub name: String,
    pub host: String,
    pub pin: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub host_name: String,
    pub pin: String,
    /// Updates per second sent to the laptop.
    pub rate: u32,
    /// 0..1
    pub rumble: f32,
    pub look: String,
    /// right | left
    pub pad_side: String,
    /// Fill the whole screen (no title bar or taskbar).
    pub fullscreen: bool,
    pub active: String,
    pub profiles: Vec<Profile>,
    /// Computers connected to before, most recent first.
    pub saved: Vec<Saved>,
    /// Program name on the laptop (lower case) to the game type to use for it.
    pub game_map: BTreeMap<String, String>,
    /// Switch game type automatically from `game_map`.
    pub auto_switch: bool,
    /// Keyboard keys Armoury Crate is set to send for the two back buttons.
    pub back_keys: Vec<u16>,
    /// While connected: put Windows in its most efficient power mode and
    /// dim the screen when touch is locked.
    pub low_power: bool,
}
impl Default for Config {
    fn default() -> Self {
        Config {
            host: String::new(),
            host_name: String::new(),
            pin: String::new(),
            rate: 250,
            rumble: 1.0,
            look: "daylight".into(),
            pad_side: "right".into(),
            fullscreen: true,
            active: "standard".into(),
            profiles: default_profiles(),
            saved: Vec::new(),
            game_map: BTreeMap::new(),
            auto_switch: true,
            back_keys: vec![0x91, 0x13], // Scroll Lock, Pause: harmless on the Ally itself
            low_power: true,
        }
    }
}

impl Config {
    pub fn active_profile(&self) -> &Profile {
        self.profiles.iter().find(|p| p.id == self.active).unwrap_or(&self.profiles[0])
    }

    /// Keep a loaded or UI-supplied config inside safe limits.
    pub fn sanitize(&mut self) {
        let defaults = default_profiles();
        for d in &defaults {
            if !self.profiles.iter().any(|p| p.id == d.id) {
                self.profiles.push(d.clone());
            }
        }
        self.profiles.retain(|p| defaults.iter().any(|d| d.id == p.id));
        if !self.profiles.iter().any(|p| p.id == self.active) {
            self.active = "standard".into();
        }
        self.rate = match self.rate {
            0..=187 => 125,
            188..=375 => 250,
            376..=750 => 500,
            _ => 1000,
        };
        self.rumble = fin(self.rumble, 1.0).clamp(0.0, 1.0);
        self.saved.truncate(8);
        self.back_keys.resize(2, 0);
        let ids: Vec<String> = self.profiles.iter().map(|p| p.id.clone()).collect();
        self.game_map.retain(|_, v| ids.contains(v));
        for p in &mut self.profiles {
            for s in [&mut p.ls, &mut p.rs] {
                s.dz = fin(s.dz, 0.08).clamp(0.0, 0.5);
                s.outer = fin(s.outer, 0.97).clamp(s.dz + 0.1, 1.0);
                s.curve = fin(s.curve, 1.0).clamp(0.5, 3.0);
                s.sens = fin(s.sens, 1.0).clamp(0.3, 2.0);
                s.anti = fin(s.anti, 0.0).clamp(0.0, 0.5);
            }
            for t in [&mut p.lt, &mut p.rt] {
                t.start = fin(t.start, 0.04).clamp(0.0, 0.8);
                t.end = fin(t.end, 0.96).clamp(t.start + 0.05, 1.0);
            }
            let m = &mut p.motion;
            m.sens = fin(m.sens, 1.0).clamp(0.1, 4.0);
            m.vratio = fin(m.vratio, 0.7).clamp(0.0, 1.5);
            m.smooth = fin(m.smooth, 0.25).clamp(0.0, 1.0);
            m.deadzone = fin(m.deadzone, 1.0).clamp(0.0, 10.0);
            m.range = fin(m.range, 35.0).clamp(10.0, 80.0);
            m.floor = fin(m.floor, 0.0).clamp(0.0, 0.4);
            p.trackpad.sens = fin(p.trackpad.sens, 1.0).clamp(0.2, 3.0);
            p.keys.resize(6, "none".into());
            p.back.resize(2, "none".into());
            p.modes.retain(|k, v| {
                ["a", "b", "x", "y", "lb", "rb", "ls", "rs", "lt", "rt"].contains(&k.as_str()) && (v == "turbo" || v == "toggle")
            });
        }
    }
}

fn fin(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("AllyPad")
}

pub fn load() -> Config {
    let mut cfg: Config = std::fs::read_to_string(dir().join("sender.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    cfg.sanitize();
    cfg
}

pub fn save(cfg: &Config) {
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let tmp = d.join("sender.json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(tmp, d.join("sender.json"));
        }
    }
}
