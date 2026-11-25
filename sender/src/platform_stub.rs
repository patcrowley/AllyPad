//! Stand-in hardware for development on other systems. With ALLYPAD_SIM=1
//! it produces a moving controller and motion signal so the whole pipeline
//! and the UI can be exercised without an Ally.

use crate::process::RawPad;
use std::time::Instant;

fn sim() -> bool {
    std::env::var("ALLYPAD_SIM").is_ok()
}

pub struct Pad {
    t0: Instant,
}
impl Pad {
    pub fn new() -> Pad {
        Pad { t0: Instant::now() }
    }
    pub fn read(&mut self, _back_keys: &[u16]) -> RawPad {
        if !sim() {
            return RawPad::default();
        }
        let t = self.t0.elapsed().as_secs_f32();
        RawPad {
            connected: true,
            buttons: if (t as u32) % 4 == 0 { 0x1000 } else { 0 },
            lt: (0.5 + 0.5 * (t * 0.9).sin()).powi(2),
            rt: 0.72,
            lx: 0.62 * (t * 1.3).cos(),
            ly: 0.48 * (t * 1.3).sin() + 0.2,
            rx: -0.35,
            ry: 0.22 * (t * 2.1).sin() + 0.3,
            extra: 0,
        }
    }
    pub fn rumble(&mut self, _large: f32, _small: f32) {}
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
    t0: Instant,
}
impl Sensors {
    pub fn start() -> Sensors {
        Sensors { t0: Instant::now() }
    }
    pub fn take(&self) -> MotionRaw {
        if !sim() {
            return MotionRaw::default();
        }
        let t = self.t0.elapsed().as_secs_f32();
        let roll = (14.0 + 9.0 * (t * 0.8).sin()).to_radians();
        let fwd = (48.0f32 + 6.0 * (t * 0.5).cos()).to_radians();
        MotionRaw {
            have_gyro: true,
            have_accel: true,
            gyro: [8.0 * (t * 1.7).sin(), -38.0 * (t * 1.1).cos(), 0.0],
            accel: [roll.sin(), -fwd.cos() * roll.cos(), -fwd.sin() * roll.cos()],
            dangle: [0.0; 3],
            dangle_dt: 0.0,
        }
    }
}

pub fn init() {}
pub fn keep_awake() {}
pub fn open_ui(url: &str) -> Option<std::process::Child> {
    println!("AllyPad UI: {url}");
    None
}

pub fn battery() -> Option<(u8, bool)> {
    if sim() {
        Some((64, false))
    } else {
        None
    }
}
pub fn low_power(on: bool) {
    println!("low power mode: {on}");
}
pub fn dim(on: bool) {
    println!("dim screen: {on}");
}
