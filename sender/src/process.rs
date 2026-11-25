//! Turns raw controller, motion and touch input into the state sent to the
//! laptop, according to the active game-type profile. Pure logic, no I/O.

use crate::config::{Profile, Stick, Trigger};
use allypad_proto::{btn, PadState, State, MAX_KEYS};

#[derive(Clone, Copy, Default, Debug)]
pub struct RawPad {
    pub connected: bool,
    pub buttons: u16,
    pub lt: f32,
    pub rt: f32,
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    /// Back buttons: bit 0 and bit 1.
    pub extra: u8,
}

/// Sensor axes follow the Windows convention with the Ally held normally:
/// X to the right, Y toward the top edge, Z out of the screen toward you.
#[derive(Clone, Copy, Default, Debug)]
pub struct MotionIn {
    pub have_gyro: bool,
    pub have_accel: bool,
    /// Angular velocity, degrees per second.
    pub gyro: [f32; 3],
    /// Direction of gravity in g (reads 0,-1,0 held upright; 0,0,-1 flat).
    pub accel: [f32; 3],
    /// Rotation integrated since the previous step, degrees.
    pub dangle: [f32; 3],
}

#[derive(Clone, Default, Debug)]
pub struct TouchIn {
    /// Pointer motion from the touchscreen trackpad since the previous step.
    pub mouse: (f32, f32),
    pub wheel: (f32, f32),
    pub buttons: u8,
    pub stick: Option<(f32, f32)>,
    pub actions: Vec<String>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Viz {
    pub raw: [f32; 6],
    pub out: [f32; 6],
    pub out_buttons: u16,
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub motion_active: bool,
}

#[derive(Default)]
pub struct Processor {
    mouse_acc: (f64, f64),
    wheel_acc: (f64, f64),
    totals: [i32; 4],
    g: [f32; 3],
    g_init: bool,
    rate_lp: (f32, f32),
    neutral_roll: f32,
    neutral_pitch: Option<f32>,
    was_active: bool,
    recenter: bool,
    clock: f64,
    prev_held: u32,
    latched: u32,
}

const WHEEL_STEP: f64 = 30.0;

fn shape(s: &Stick, x: f32, y: f32) -> (f32, f32) {
    let mag = (x * x + y * y).sqrt();
    if mag <= s.dz || mag < 1e-5 {
        return (0.0, 0.0);
    }
    let span = (s.outer - s.dz).max(0.05);
    let mut t = ((mag - s.dz) / span).clamp(0.0, 1.0);
    t = (t.powf(s.curve) * s.sens).clamp(0.0, 1.0);
    if s.anti > 0.0 {
        t = s.anti + (1.0 - s.anti) * t;
    }
    let (ux, uy) = (x / mag, y / mag);
    (ux * t, if s.invert_y { -uy * t } else { uy * t })
}

fn trigger(t: &Trigger, v: f32) -> f32 {
    if v <= t.start {
        0.0
    } else if t.digital {
        1.0
    } else {
        ((v - t.start) / (t.end - t.start).max(0.05)).clamp(0.0, 1.0)
    }
}

/// Fade out values below `dz` instead of cutting them, so slow deliberate
/// motion still registers.
fn soften(v: f32, mag: f32, dz: f32) -> f32 {
    if dz > 0.0 && mag < dz {
        v * (mag / dz)
    } else {
        v
    }
}

fn cut(v: f32, dz: f32) -> f32 {
    v.signum() * (v.abs() - dz).max(0.0)
}

fn wrap180(mut a: f32) -> f32 {
    while a > 180.0 {
        a -= 360.0;
    }
    while a < -180.0 {
        a += 360.0;
    }
    a
}

fn swap_bits(b: u16, x: u16, y: u16) -> u16 {
    let (hx, hy) = (b & x != 0, b & y != 0);
    let mut out = b & !(x | y);
    if hx {
        out |= y;
    }
    if hy {
        out |= x;
    }
    out
}

// Bits used for per-button modes: the XInput button bits plus two for the triggers.
const M_LT: u32 = 1 << 16;
const M_RT: u32 = 1 << 17;
const MODE_NAMES: [(&str, u32); 10] = [
    ("a", btn::A as u32),
    ("b", btn::B as u32),
    ("x", btn::X as u32),
    ("y", btn::Y as u32),
    ("lb", btn::LB as u32),
    ("rb", btn::RB as u32),
    ("ls", btn::LS as u32),
    ("rs", btn::RS as u32),
    ("lt", M_LT),
    ("rt", M_RT),
];
/// Turbo presses per second.
const TURBO_HZ: f64 = 12.0;

/// One action string into held keys, controller bits and mouse buttons.
fn apply_action(a: &str, keys: &mut Vec<u16>, pad_bits: &mut u16, mouse: &mut u8) {
    let Some((k, v)) = a.split_once(':') else { return };
    match k {
        // "key:18+9" holds several keys at once (Alt+Tab).
        "key" => {
            for part in v.split('+') {
                match part.parse::<u16>() {
                    Ok(n) if n > 0 && n < 256 => keys.push(n),
                    _ => {}
                }
            }
        }
        "pad" => *pad_bits |= v.parse::<u16>().unwrap_or(0),
        "mouse" => *mouse |= v.parse::<u8>().unwrap_or(0) & 7,
        _ => {}
    }
}

fn to_axis(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

impl Processor {
    pub fn new() -> Processor {
        Processor::default()
    }

    /// Forget toggled buttons (used when the game type changes).
    pub fn reset_latches(&mut self) {
        self.latched = 0;
    }

    /// Treat the current tilt as "level".
    pub fn request_recenter(&mut self) {
        self.recenter = true;
    }

    pub fn step(&mut self, p: &Profile, raw: &RawPad, m: &MotionIn, touch: &TouchIn, dt: f32) -> (State, Viz) {
        let dt = dt.clamp(0.0002, 0.1);
        let mut viz = Viz { raw: [raw.lx, raw.ly, raw.rx, raw.ry, raw.lt, raw.rt], ..Viz::default() };

        // ---- turbo and toggle ----
        // `phys` is what the fingers are doing; `raw` becomes what counts.
        let phys = raw;
        let mut raw = *raw;
        self.clock += dt as f64;
        if !p.modes.is_empty() {
            let held = phys.buttons as u32 | if phys.lt > 0.5 { M_LT } else { 0 } | if phys.rt > 0.5 { M_RT } else { 0 };
            let pressed = held & !self.prev_held;
            let turbo_on = (self.clock * TURBO_HZ * 2.0) as u64 % 2 == 0;
            let mut eff = held;
            for (name, bit) in MODE_NAMES {
                match p.modes.get(name).map(String::as_str) {
                    Some("turbo") => {
                        if !turbo_on {
                            eff &= !bit;
                        }
                    }
                    Some("toggle") => {
                        if pressed & bit != 0 {
                            self.latched ^= bit;
                        }
                        eff = (eff & !bit) | (self.latched & bit);
                    }
                    _ => self.latched &= !bit,
                }
            }
            let moded: u32 = MODE_NAMES.iter().filter(|(n, _)| p.modes.contains_key(*n)).map(|(_, b)| *b).sum();
            raw.buttons = (raw.buttons & !(moded as u16)) | (eff & moded & 0xFFFF) as u16;
            if moded & M_LT != 0 {
                raw.lt = if eff & M_LT != 0 { phys.lt.max(if self.latched & M_LT != 0 { 1.0 } else { 0.0 }) } else { 0.0 };
            }
            if moded & M_RT != 0 {
                raw.rt = if eff & M_RT != 0 { phys.rt.max(if self.latched & M_RT != 0 { 1.0 } else { 0.0 }) } else { 0.0 };
            }
            self.prev_held = held;
        } else {
            self.latched = 0;
            self.prev_held = 0;
        }
        let raw = &raw;

        let (mut lx, mut ly) = shape(&p.ls, raw.lx, raw.ly);
        let (mut rx, mut ry) = shape(&p.rs, raw.rx, raw.ry);
        let lt = trigger(&p.lt, raw.lt);
        let rt = trigger(&p.rt, raw.rt);
        let mut buttons = raw.buttons;

        // ---- motion ----
        let mode = p.motion.mode.as_str();
        let mo = &p.motion;
        if m.have_accel {
            let n = (m.accel[0].powi(2) + m.accel[1].powi(2) + m.accel[2].powi(2)).sqrt();
            // Ignore readings taken mid-shake; they say little about "down".
            if n > 0.5 && n < 1.6 {
                let tau = 0.03 + mo.smooth * 0.25;
                let a = if self.g_init { dt / (tau + dt) } else { 1.0 };
                for i in 0..3 {
                    self.g[i] += (m.accel[i] / n - self.g[i]) * a;
                }
                self.g_init = true;
            }
        }
        let gn = (self.g[0].powi(2) + self.g[1].powi(2) + self.g[2].powi(2)).sqrt();
        let g = if self.g_init && gn > 0.2 { [self.g[0] / gn, self.g[1] / gn, self.g[2] / gn] } else { [0.0, -1.0, 0.0] };
        let roll_abs = g[0].clamp(-1.0, 1.0).asin().to_degrees();
        let fwd_abs = (-g[2]).atan2(-g[1]).to_degrees();

        let turn = |w: &[f32; 3]| -> f32 {
            match mo.axis.as_str() {
                "yaw" => -w[1],
                "roll" => -w[2],
                _ => w[0] * g[0] + w[1] * g[1] + w[2] * g[2],
            }
        };
        let (yaw_rate, pitch_rate) = (turn(&m.gyro), m.gyro[0]);
        viz.yaw = if m.have_gyro { yaw_rate } else { 0.0 };

        let needs_gyro = matches!(mode, "aim_stick" | "aim_mouse");
        let needs_accel = matches!(mode, "steer" | "tilt");
        let gate = match mo.when.as_str() {
            "lt" => phys.lt > 0.25,
            "rt" => phys.rt > 0.25,
            "lb" => phys.buttons & btn::LB != 0,
            "rb" => phys.buttons & btn::RB != 0,
            _ => true,
        };
        let active = gate && ((needs_gyro && m.have_gyro) || (needs_accel && m.have_accel && self.g_init));
        if self.recenter && self.g_init {
            self.neutral_roll = roll_abs;
            self.neutral_pitch = Some(fwd_abs);
            self.recenter = false;
        }
        if active && (!self.was_active || self.neutral_pitch.is_none()) {
            // Each fresh activation takes the current forward tilt as neutral.
            self.neutral_pitch = Some(fwd_abs);
        }
        self.was_active = active;
        viz.motion_active = active;
        let roll = wrap180(roll_abs - self.neutral_roll);
        let pitch = wrap180(fwd_abs - self.neutral_pitch.unwrap_or(fwd_abs));
        viz.roll = roll;
        viz.pitch = pitch;

        let sx = if mo.invert_x { -1.0 } else { 1.0 };
        let sy = if mo.invert_y { -1.0 } else { 1.0 };
        let mut motion_mouse = (0.0f32, 0.0f32);
        if active {
            match mode {
                "aim_stick" => {
                    let tau = mo.smooth * 0.08;
                    let a = dt / (tau + dt);
                    self.rate_lp.0 += (yaw_rate - self.rate_lp.0) * a;
                    self.rate_lp.1 += (pitch_rate - self.rate_lp.1) * a;
                    let mag = (self.rate_lp.0.powi(2) + self.rate_lp.1.powi(2)).sqrt();
                    let mut vx = soften(self.rate_lp.0, mag, mo.deadzone) * mo.sens / 150.0 * sx;
                    let mut vy = soften(self.rate_lp.1, mag, mo.deadzone) * mo.sens * mo.vratio / 150.0 * sy;
                    let vm = (vx * vx + vy * vy).sqrt();
                    if vm > 0.004 {
                        let boosted = (mo.floor + (1.0 - mo.floor) * vm.min(1.0)) / vm;
                        vx *= boosted;
                        vy *= boosted;
                    }
                    rx = (rx + vx).clamp(-1.0, 1.0);
                    ry = (ry + vy).clamp(-1.0, 1.0);
                }
                "aim_mouse" => {
                    let mag = (yaw_rate.powi(2) + pitch_rate.powi(2)).sqrt();
                    let px = 14.0 * mo.sens;
                    motion_mouse.0 = soften(turn(&m.dangle), mag, mo.deadzone) * px * sx;
                    motion_mouse.1 = -soften(m.dangle[0], mag, mo.deadzone) * px * mo.vratio * sy;
                }
                "steer" | "tilt" => {
                    lx = (lx + cut(roll, mo.deadzone) / mo.range * mo.sens * sx).clamp(-1.0, 1.0);
                    if mode == "tilt" {
                        ly = (ly + cut(pitch, mo.deadzone) / mo.range * mo.sens * sy).clamp(-1.0, 1.0);
                    }
                }
                _ => {}
            }
        } else {
            self.rate_lp = (0.0, 0.0);
        }

        // ---- touchscreen stick ----
        if p.trackpad.mode == "stick" {
            if let Some((x, y)) = touch.stick {
                rx = (rx + x).clamp(-1.0, 1.0);
                ry = (ry + y).clamp(-1.0, 1.0);
            }
        }

        // ---- button options ----
        if p.ls_dpad {
            if (lx * lx + ly * ly).sqrt() > 0.5 {
                let oct = (ly.atan2(lx).to_degrees() / 45.0).round() as i32;
                buttons |= match oct.rem_euclid(8) {
                    0 => btn::RIGHT,
                    1 => btn::RIGHT | btn::UP,
                    2 => btn::UP,
                    3 => btn::UP | btn::LEFT,
                    4 => btn::LEFT,
                    5 => btn::LEFT | btn::DOWN,
                    6 => btn::DOWN,
                    _ => btn::DOWN | btn::RIGHT,
                };
            }
            lx = 0.0;
            ly = 0.0;
        }
        if p.swap_sticks {
            std::mem::swap(&mut lx, &mut rx);
            std::mem::swap(&mut ly, &mut ry);
            buttons = swap_bits(buttons, btn::LS, btn::RS);
        }
        if p.swap_ab {
            buttons = swap_bits(buttons, btn::A, btn::B);
            buttons = swap_bits(buttons, btn::X, btn::Y);
        }

        // ---- touch keys ----
        let mut keys: Vec<u16> = Vec::new();
        let mut mouse_buttons = touch.buttons & 7;
        let mut touch_pad_bits = 0u16;
        for a in &touch.actions {
            apply_action(a, &mut keys, &mut touch_pad_bits, &mut mouse_buttons);
        }
        for (i, a) in p.back.iter().enumerate().take(2) {
            if phys.extra >> i & 1 == 1 {
                apply_action(a, &mut keys, &mut touch_pad_bits, &mut mouse_buttons);
            }
        }

        // ---- pointer ----
        self.mouse_acc.0 += (touch.mouse.0 + motion_mouse.0) as f64;
        self.mouse_acc.1 += (touch.mouse.1 + motion_mouse.1) as f64;
        self.wheel_acc.0 += touch.wheel.0 as f64;
        self.wheel_acc.1 += touch.wheel.1 as f64;

        viz.out = [lx, ly, rx, ry, lt, rt];
        let mut pad = PadState::default();
        if p.output == "desktop" {
            let d = dt as f64;
            self.mouse_acc.0 += lx as f64 * 1100.0 * d;
            self.mouse_acc.1 -= ly as f64 * 1100.0 * d;
            self.wheel_acc.0 += ry as f64 * 1500.0 * d;
            self.wheel_acc.1 += rx as f64 * 1500.0 * d;
            let map: [(u16, u16); 10] = [
                (btn::X, 13),
                (btn::Y, 27),
                (btn::UP, 38),
                (btn::DOWN, 40),
                (btn::LEFT, 37),
                (btn::RIGHT, 39),
                (btn::LB, 0xA6),
                (btn::RB, 0xA7),
                (btn::START, 0x5B),
                (btn::BACK, 9),
            ];
            for (b, vk) in map {
                if buttons & b != 0 {
                    keys.push(vk);
                }
            }
            if buttons & btn::A != 0 || rt > 0.5 {
                mouse_buttons |= 1;
            }
            if buttons & btn::B != 0 {
                mouse_buttons |= 2;
            }
            if buttons & btn::LS != 0 {
                mouse_buttons |= 4;
            }
            pad.buttons = touch_pad_bits;
        } else {
            pad = PadState {
                buttons: buttons | touch_pad_bits,
                lt: (lt * 255.0).round() as u8,
                rt: (rt * 255.0).round() as u8,
                lx: to_axis(lx),
                ly: to_axis(ly),
                rx: to_axis(rx),
                ry: to_axis(ry),
            };
        }
        viz.out_buttons = pad.buttons;

        let take = |acc: &mut f64, step: f64| -> i32 {
            let n = (*acc / step).trunc();
            *acc -= n * step;
            (n * step) as i32
        };
        self.totals[0] = self.totals[0].wrapping_add(take(&mut self.mouse_acc.0, 1.0));
        self.totals[1] = self.totals[1].wrapping_add(take(&mut self.mouse_acc.1, 1.0));
        self.totals[2] = self.totals[2].wrapping_add(take(&mut self.wheel_acc.0, WHEEL_STEP));
        self.totals[3] = self.totals[3].wrapping_add(take(&mut self.wheel_acc.1, WHEEL_STEP));

        let mut st = State {
            pad,
            mouse_x: self.totals[0],
            mouse_y: self.totals[1],
            wheel: self.totals[2],
            hwheel: self.totals[3],
            mouse_buttons,
            ..State::default()
        };
        keys.sort_unstable();
        keys.dedup();
        for (slot, k) in st.keys.iter_mut().zip(keys.iter().take(MAX_KEYS)) {
            *slot = *k;
        }
        (st, viz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{default_profiles, Profile};

    fn prof(id: &str) -> Profile {
        default_profiles().into_iter().find(|p| p.id == id).unwrap()
    }
    fn run(p: &Profile, raw: RawPad, m: MotionIn, t: TouchIn) -> (State, Viz) {
        let mut pr = Processor::new();
        let mut out = pr.step(p, &raw, &m, &t, 0.004);
        for _ in 0..500 {
            out = pr.step(p, &raw, &m, &t, 0.004);
        }
        out
    }
    fn upright() -> MotionIn {
        MotionIn { have_gyro: true, have_accel: true, accel: [0.0, -1.0, 0.0], ..Default::default() }
    }

    #[test]
    fn dead_zone_and_full_travel() {
        let p = prof("standard");
        let (s, _) = run(&p, RawPad { lx: 0.05, ..Default::default() }, MotionIn::default(), TouchIn::default());
        assert_eq!(s.pad.lx, 0);
        let (s, _) = run(&p, RawPad { lx: 1.0, ry: -1.0, lt: 1.0, ..Default::default() }, MotionIn::default(), TouchIn::default());
        assert_eq!((s.pad.lx, s.pad.ry, s.pad.lt), (32767, -32767, 255));
        // diagonal keeps its direction
        let (s, _) = run(&p, RawPad { lx: 0.5, ly: 0.5, ..Default::default() }, MotionIn::default(), TouchIn::default());
        assert_eq!(s.pad.lx, s.pad.ly);
        assert!(s.pad.lx > 15000 && s.pad.lx < 24000);
    }

    #[test]
    fn curve_boost_and_hair_trigger() {
        let mut p = prof("standard");
        p.rs.curve = 2.0;
        let lin = run(&prof("standard"), RawPad { rx: 0.5, ..Default::default() }, MotionIn::default(), TouchIn::default()).0.pad.rx;
        let fine = run(&p, RawPad { rx: 0.5, ..Default::default() }, MotionIn::default(), TouchIn::default()).0.pad.rx;
        assert!(fine < lin / 2 + 2000 && fine > 0);
        p.rs.anti = 0.2;
        let boosted = run(&p, RawPad { rx: 0.1, ..Default::default() }, MotionIn::default(), TouchIn::default()).0.pad.rx;
        assert!(boosted > (0.2 * 32767.0) as i16);
        p.rt.digital = true;
        assert_eq!(run(&p, RawPad { rt: 0.2, ..Default::default() }, MotionIn::default(), TouchIn::default()).0.pad.rt, 255);
    }

    #[test]
    fn button_options() {
        let mut p = prof("fighting");
        let (s, _) = run(&p, RawPad { lx: -0.7, ly: 0.7, buttons: btn::A, ..Default::default() }, MotionIn::default(), TouchIn::default());
        assert_eq!(s.pad.buttons, btn::A | btn::UP | btn::LEFT);
        assert_eq!((s.pad.lx, s.pad.ly), (0, 0));
        p.swap_ab = true;
        let (s, _) = run(&p, RawPad { buttons: btn::A | btn::Y, ..Default::default() }, MotionIn::default(), TouchIn::default());
        assert_eq!(s.pad.buttons, btn::B | btn::X);
        let mut p = prof("standard");
        p.swap_sticks = true;
        let (s, _) = run(&p, RawPad { lx: 1.0, buttons: btn::LS, ..Default::default() }, MotionIn::default(), TouchIn::default());
        assert_eq!((s.pad.lx, s.pad.rx, s.pad.buttons), (0, 32767, btn::RS));
    }

    #[test]
    fn tilt_steering() {
        let p = prof("racing");
        // right edge down by 20 degrees
        let a = 20f32.to_radians();
        let m = MotionIn { accel: [a.sin(), -a.cos(), 0.0], ..upright() };
        let (s, v) = run(&p, RawPad::default(), m, TouchIn::default());
        assert!((v.roll - 20.0).abs() < 0.5, "roll {}", v.roll);
        let want = (20.0 - 1.5) / 35.0;
        assert!((s.pad.lx as f32 / 32767.0 - want).abs() < 0.02);
        // works lying flat too
        let m = MotionIn { accel: [-a.sin(), 0.0, -a.cos()], ..upright() };
        let (s, _) = run(&p, RawPad::default(), m, TouchIn::default());
        assert!(s.pad.lx < -15000);
    }

    #[test]
    fn flight_takes_first_hold_as_neutral() {
        let p = prof("flight");
        let hold = |deg: f32| {
            let a = deg.to_radians();
            MotionIn { accel: [0.0, -a.cos(), -a.sin()], ..upright() }
        };
        let mut pr = Processor::new();
        for _ in 0..300 {
            pr.step(&p, &RawPad::default(), &hold(40.0), &TouchIn::default(), 0.004);
        }
        let (s, _) = pr.step(&p, &RawPad::default(), &hold(40.0), &TouchIn::default(), 0.004);
        assert_eq!(s.pad.ly, 0);
        let mut s = s;
        for _ in 0..600 {
            s = pr.step(&p, &RawPad::default(), &hold(60.0), &TouchIn::default(), 0.004).0;
        }
        assert!(s.pad.ly > 15000, "tipping forward pushes the stick forward: {}", s.pad.ly);
    }

    #[test]
    fn gyro_aim_only_while_trigger_held() {
        let p = prof("shooter");
        let m = MotionIn { gyro: [0.0, -60.0, 0.0], ..upright() }; // turning right
        let (s, v) = run(&p, RawPad::default(), m, TouchIn::default());
        assert!(!v.motion_active);
        assert_eq!(s.pad.rx, 0);
        let (s, v) = run(&p, RawPad { lt: 1.0, ..Default::default() }, m, TouchIn::default());
        assert!(v.motion_active);
        let rx = s.pad.rx as f32 / 32767.0;
        assert!(rx > 0.4 && rx < 0.6, "rx {rx}");
        // same turn with the device flat on a table: the turn is now around Z
        let flat = MotionIn { gyro: [0.0, 0.0, -60.0], accel: [0.0, 0.0, -1.0], ..upright() };
        let (s2, _) = run(&p, RawPad { lt: 1.0, ..Default::default() }, flat, TouchIn::default());
        assert!((s2.pad.rx - s.pad.rx).abs() < 400);
    }

    #[test]
    fn gyro_mouse_and_trackpad_accumulate_without_loss() {
        let mut p = prof("standard");
        p.motion.mode = "aim_mouse".into();
        p.motion.deadzone = 0.0;
        let mut pr = Processor::new();
        let m = MotionIn { gyro: [10.0, -20.0, 0.0], dangle: [0.01, -0.02, 0.0], ..upright() };
        let t = TouchIn { mouse: (0.3, 0.0), ..Default::default() };
        let mut s = State::default();
        for _ in 0..1000 {
            s = pr.step(&p, &RawPad::default(), &m, &t, 0.001).0;
        }
        // 20 degrees right * 14 px/deg + 300 px of trackpad
        assert!((s.mouse_x - 580).abs() <= 2, "x {}", s.mouse_x);
        // 10 degrees up * 14 * 0.7, and up is negative y
        assert!((s.mouse_y + 98).abs() <= 2, "y {}", s.mouse_y);
    }

    #[test]
    fn toggle_and_turbo() {
        let mut p = prof("standard");
        p.modes.insert("rt".into(), "toggle".into());
        p.modes.insert("a".into(), "turbo".into());
        let mut pr = Processor::new();
        let none = RawPad::default();
        let press = RawPad { rt: 0.9, ..Default::default() };
        let step = |pr: &mut Processor, r: &RawPad| pr.step(&p, r, &MotionIn::default(), &TouchIn::default(), 0.004).0;
        assert_eq!(step(&mut pr, &none).pad.rt, 0);
        assert_eq!(step(&mut pr, &press).pad.rt, 255); // squeeze: on
        assert_eq!(step(&mut pr, &press).pad.rt, 255);
        assert_eq!(step(&mut pr, &none).pad.rt, 255); // let go: stays on
        assert_eq!(step(&mut pr, &none).pad.rt, 255);
        assert_eq!(step(&mut pr, &press).pad.rt, 0); // squeeze again: off
        assert_eq!(step(&mut pr, &none).pad.rt, 0);

        // turbo: holding A gives a steady stream of presses
        let a = RawPad { buttons: btn::A | btn::B, ..Default::default() };
        let (mut ups, mut downs, mut last) = (0, 0, false);
        for _ in 0..250 {
            let s = step(&mut pr, &a);
            assert!(s.pad.buttons & btn::B != 0, "other buttons are untouched");
            let on = s.pad.buttons & btn::A != 0;
            if on && !last {
                downs += 1;
            }
            if !on && last {
                ups += 1;
            }
            last = on;
        }
        assert!((11..=13).contains(&downs) && (11..=13).contains(&ups), "{downs} presses in one second");
    }

    #[test]
    fn chords_and_back_buttons() {
        let mut p = prof("standard");
        p.back = vec!["key:18+9".into(), "pad:1024".into()];
        let mut pr = Processor::new();
        let t = TouchIn { actions: vec!["key:91+44".into()], ..Default::default() };
        let (s, _) = pr.step(&p, &RawPad { extra: 0b11, ..Default::default() }, &MotionIn::default(), &t, 0.004);
        for vk in [18, 9, 91, 44] {
            assert!(s.keys.contains(&vk), "missing {vk}");
        }
        assert_eq!(s.pad.buttons, btn::GUIDE);
        let (s, _) = pr.step(&p, &RawPad::default(), &MotionIn::default(), &TouchIn::default(), 0.004);
        assert_eq!((s.keys, s.pad.buttons), ([0; MAX_KEYS], 0));
    }

    #[test]
    fn desktop_mode_and_touch_keys() {
        let p = prof("desktop");
        let t = TouchIn { actions: vec!["key:27".into(), "pad:1024".into(), "mouse:4".into()], ..Default::default() };
        let mut pr = Processor::new();
        let raw = RawPad { lx: 1.0, buttons: btn::A | btn::UP, ..Default::default() };
        let mut s = State::default();
        for _ in 0..250 {
            s = pr.step(&p, &raw, &MotionIn::default(), &t, 0.004).0;
        }
        assert!((s.mouse_x - 1100).abs() < 10);
        assert_eq!(s.mouse_buttons, 1 | 4);
        assert!(s.keys.contains(&27) && s.keys.contains(&38));
        assert_eq!(s.pad.buttons, btn::GUIDE);
        assert_eq!(s.pad.lx, 0);
    }
}
