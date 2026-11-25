//! Non-Windows stand-in used for development and tests: prints what it
//! would do. Set ALLYPAD_TRACE=1 to see every event.

use crate::core::Sink;
use allypad_proto::PadState;

pub struct StubSink {
    trace: bool,
    pub pad_error: Option<String>,
}

impl StubSink {
    pub fn new() -> StubSink {
        StubSink { trace: std::env::var("ALLYPAD_TRACE").is_ok(), pad_error: None }
    }
    pub fn retry_driver(&mut self) {}
    fn t(&self, s: String) {
        if self.trace {
            println!("{s}");
        }
    }
}

impl Sink for StubSink {
    fn pad(&mut self, slot: usize, p: &PadState) {
        let _ = slot;
        self.t(format!("pad {:04x} lt{} rt{} L({},{}) R({},{})", p.buttons, p.lt, p.rt, p.lx, p.ly, p.rx, p.ry));
    }
    fn pad_release(&mut self, _slot: usize) {
        self.t("unplug".into());
    }
    fn mouse_move(&mut self, dx: i32, dy: i32) {
        self.t(format!("move {dx} {dy}"));
    }
    fn wheel(&mut self, v: i32, h: i32) {
        self.t(format!("wheel {v} {h}"));
    }
    fn mouse_button(&mut self, bit: u8, down: bool) {
        self.t(format!("mb {bit} {down}"));
    }
    fn key(&mut self, vk: u16, down: bool) {
        self.t(format!("key {vk} {down}"));
    }
    fn rumble(&mut self, _slot: usize) -> (u8, u8) {
        (0, 0)
    }
    fn pad_ok(&self) -> bool {
        true
    }
    fn keep_awake(&mut self, _on: bool) {}
}
