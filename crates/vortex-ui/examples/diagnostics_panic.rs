//! Browser-only fixture for checking that a WASM panic reports its location
//! without reporting its payload. This is not included in the application UI.
//! The fixture page decides when to call `trigger_panic` so both the probing
//! (bootstrap hand-off) and enabled (sendBeacon) delivery paths can be tested.

#[cfg(target_arch = "wasm32")]
fn main() {
    vortex_ui::diagnostics::init();
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn trigger_panic() {
    panic!("WASM_DIAGNOSTIC_PANIC_PAYLOAD_SENTINEL");
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
