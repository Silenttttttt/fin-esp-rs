//! KY-022 IR remote receiver -- NEC protocol, single-wire, bit-banged, no external crate.
//! Same hand-rolled-driver approach as dht22.rs, for the same reason: precise timing loops
//! run directly against this project's own `esp_timer_get_time()` primitive rather than an
//! unfamiliar crate. Button-to-code mapping and the full bench-test debugging history (GPIO2/5
//! boot-strap trap, the real root cause of the flaky serial-vs-firmware saga) live in
//! `esp32-tests/ir_receiver_test/` in the sibling Fin-ESP repo -- this module only implements
//! decode + dispatch, not the discovery work.
//!
//! Wiring: receiver S -> GPIO4 (confirmed safe, non-strapping, already bench-tested extensively
//! on identical hardware), VCC -> 3.3V (not 5V -- see that repo's NOTES.md for why), GND -> GND.

use esp_idf_hal::gpio::{Input, PinDriver, Pull};
use log::{info, warn};
use std::sync::Arc;
use std::time::Duration;

use crate::led::LedState;
use crate::tuya::{LampHandle, LAMP_UNKNOWN};

#[derive(Debug)]
enum IrError {
    Timeout,
    ChecksumMismatch,
}

fn now_us() -> i64 {
    unsafe { esp_idf_sys::esp_timer_get_time() }
}

/// Busy-waits while the pin reads `high`, returning elapsed microseconds once it flips, or
/// `Timeout` if it never does within `timeout_us`. Same shape as dht22.rs's own helper.
fn wait_while(pin: &PinDriver<'_, Input>, high: bool, timeout_us: i64) -> Result<i64, IrError> {
    let start = now_us();
    loop {
        if pin.is_high() != high {
            return Ok(now_us() - start);
        }
        if now_us() - start > timeout_us {
            return Err(IrError::Timeout);
        }
    }
}

/// Reads one NEC frame. Caller has already confirmed the pin is LOW (header mark started).
/// NEC timing (microseconds): header mark ~9000 low, header space ~4500 high, then 32 bits
/// each a ~562us low mark followed by a high space whose length encodes the bit -- ~562us for
/// a 0, ~1687us for a 1. Address and command each arrive LSB-first with their own bitwise
/// complement immediately after, for a cheap correctness check.
fn read_nec_frame(pin: &PinDriver<'_, Input>) -> Result<(u8, u8), IrError> {
    wait_while(pin, false, 11_000)?; // end of header mark
    wait_while(pin, true, 6_000)?; // end of header space

    let mut bytes = [0u8; 4];
    for i in 0..32 {
        wait_while(pin, false, 700)?; // consume the fixed-length bit mark
        let space_us = wait_while(pin, true, 2_200)?; // bit-encoding space
        let bit = if space_us > 1_000 { 1u8 } else { 0u8 };
        bytes[i / 8] = (bytes[i / 8] >> 1) | (bit << 7);
    }

    let (address, address_inv, command, command_inv) = (bytes[0], bytes[1], bytes[2], bytes[3]);
    if address != !address_inv || command != !command_inv {
        return Err(IrError::ChecksumMismatch);
    }
    Ok((address, command))
}

/// Nudges the lamp's custom-white brightness/temp by `delta`, clamped to the Tuya range,
/// carrying the other axis forward unchanged. Falls back to a mid-point start (500) the first
/// time either axis is touched, matching LAMP_UNKNOWN's "never set" meaning elsewhere.
fn step_brightness_temp(lamp_handle: &LampHandle, brightness_delta: i32, temp_delta: i32) {
    use std::sync::atomic::Ordering;
    let cur_b = lamp_handle.last_brightness.load(Ordering::Relaxed);
    let cur_b = if cur_b == LAMP_UNKNOWN { 500 } else { cur_b };
    let cur_t = lamp_handle.last_temp.load(Ordering::Relaxed);
    let cur_t = if cur_t == LAMP_UNKNOWN { 500 } else { cur_t };
    let new_b = (cur_b as i32 + brightness_delta).clamp(10, 1000) as u16;
    let new_t = (cur_t as i32 + temp_delta).clamp(0, 1000) as u16;
    lamp_handle.queue_brightness_temp(new_b, new_t);
}

// Real bug found live (2026-09-22): queue_brightness_temp() always sends "21":"white" (see
// LampHandle::poll()'s target==4 arm in tuya/mod.rs), so stepping "brightness" while a color
// was active silently forced the bulb back to white mode, discarding the color. LampHandle
// doesn't expose which mode is currently active, so this thread tracks it locally -- true
// whenever THIS remote's own digit buttons last picked a color, false whenever it last picked
// a white-mode preset/toggle. Not perfectly authoritative if something else (physical
// buttons, web UI) changes the mode in between, but correct for the actual reported case:
// using this same remote to pick a color, then adjust its brightness with the same remote.
static IN_COLOUR_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_HUE: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
static LAST_VAL: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(1000);

/// Nudges the colour mode's V (value/brightness in HSV) by `delta`, keeping the current hue
/// and full saturation, so "brightness" while on a color dims/brightens that color instead of
/// switching to white.
fn step_colour_value(lamp_handle: &LampHandle, delta: i32) {
    use std::sync::atomic::Ordering;
    let hue = LAST_HUE.load(Ordering::Relaxed);
    let cur_val = LAST_VAL.load(Ordering::Relaxed);
    let new_val = (cur_val as i32 + delta).clamp(10, 1000) as u16;
    LAST_VAL.store(new_val, Ordering::Relaxed);
    lamp_handle.queue_colour(hue, 1000, new_val);
}

const STEP: i32 = 80; // ~8% of the 10-1000 Tuya range per press -- a few presses to notice, not one

// This desktop's own media/volume machine_id -- the laptop bridge protocol identifies
// machines by socket.gethostname() (see laptop/mic_key_daemon.py / play_pause_server.py in
// the sibling Fin-ESP repo), confirmed live as "silent-ms7e56". queue_volume_for_machine()
// takes an absolute 0-153 value (matching VOLUME_PCT's own internal range), not a delta, so
// this thread tracks its own last-set value locally to step from.
const DESKTOP_MACHINE_ID: &str = "silent-ms7e56";
const VOLUME_STEP: i32 = 11; // ~7% of the 0-153 range per press
static DESKTOP_VOLUME: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(76); // ~50% start

fn step_desktop_volume(delta: i32) {
    use std::sync::atomic::Ordering;
    let cur = DESKTOP_VOLUME.load(Ordering::Relaxed);
    let new_vol = (cur as i32 + delta).clamp(0, 153) as u8;
    DESKTOP_VOLUME.store(new_vol, Ordering::Relaxed);
    crate::queue_volume_for_machine(DESKTOP_MACHINE_ID, new_vol);
}

/// Hue for each digit button's color assignment (full saturation/value for vivid, clearly
/// distinguishable colors) -- an 8-step rainbow spread, 40 degrees apart. Button 1 (0x45) is
/// intentionally NOT here -- it mirrors the physical black button (GPIO14) instead, which
/// does nothing but its own red-LED flash, so button 1 gets no color and no other action,
/// same as that button. Colors shifted down one slot from the original 9-button assignment.
fn digit_hue(command: u8) -> Option<u16> {
    match command {
        0x46 => Some(0),   // 2: red
        0x47 => Some(40),  // 3: orange
        0x44 => Some(80),  // 4: yellow-green
        0x40 => Some(120), // 5: green
        0x43 => Some(160), // 6: teal
        0x07 => Some(200), // 7: cyan-blue
        0x15 => Some(240), // 8: blue
        0x09 => Some(280), // 9: purple
        _ => None,
    }
}

/// Dispatches a decoded button press to the same LampHandle/media actions the physical
/// buttons and web UI already use -- this remote is a second way to trigger existing
/// behavior, not a parallel system. See esp32-tests/ir_receiver_test/remote_button_codes.md
/// for exactly how each command byte was captured and cross-verified against this exact
/// remote (bench-tested on identical hardware before this was wired into the real device).
fn dispatch(command: u8, lamp_handle: &LampHandle, play_pause_ready: &'static std::sync::atomic::AtomicBool) {
    match command {
        0x1C => {
            // ok -- toggle
            let current_on = lamp_handle.display_state().on;
            lamp_handle.flip_target(current_on);
            IN_COLOUR_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
            info!("[ir] ok -> lamp toggle");
        }
        0x16 => {
            // *
            lamp_handle.queue_warm_dim();
            IN_COLOUR_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
            info!("[ir] * -> warm dim preset");
        }
        0x0D => {
            // #
            lamp_handle.queue_bright_white();
            IN_COLOUR_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
            info!("[ir] # -> bright white preset");
        }
        0x18 => {
            // up -- steps colour's V if a color is active, brightness otherwise (see
            // IN_COLOUR_MODE's own comment for why this distinction is necessary)
            if IN_COLOUR_MODE.load(std::sync::atomic::Ordering::Relaxed) {
                step_colour_value(lamp_handle, STEP);
            } else {
                step_brightness_temp(lamp_handle, STEP, 0);
            }
            info!("[ir] up -> brightness+");
        }
        0x52 => {
            // down
            if IN_COLOUR_MODE.load(std::sync::atomic::Ordering::Relaxed) {
                step_colour_value(lamp_handle, -STEP);
            } else {
                step_brightness_temp(lamp_handle, -STEP, 0);
            }
            info!("[ir] down -> brightness-");
        }
        0x5A => {
            // right -- was color temp warmer, now desktop volume up
            step_desktop_volume(VOLUME_STEP);
            info!("[ir] right -> desktop volume+");
        }
        0x08 => {
            // left -- was color temp cooler, now desktop volume down
            step_desktop_volume(-VOLUME_STEP);
            info!("[ir] left -> desktop volume-");
        }
        0x19 => {
            // 0 -- moved here from 5
            play_pause_ready.store(true, std::sync::atomic::Ordering::Relaxed);
            info!("[ir] 0 -> media play/pause");
        }
        0x45 => {
            // 1 -- mirrors the physical black button (GPIO14): no lamp action at all,
            // just the universal red-LED flash every button already gets above.
            info!("[ir] 1 -> (matches black button: flash only)");
        }
        _ => {
            // Remaining digit buttons (2-9) each set the lamp to a distinct color -- see
            // digit_hue()'s own comment for the rainbow spread.
            if let Some(hue) = digit_hue(command) {
                use std::sync::atomic::Ordering;
                LAST_HUE.store(hue, Ordering::Relaxed);
                LAST_VAL.store(1000, Ordering::Relaxed);
                IN_COLOUR_MODE.store(true, Ordering::Relaxed);
                lamp_handle.queue_colour(hue, 1000, 1000);
                info!("[ir] digit -> color hue={hue}");
            }
        }
    }
}

pub fn spawn_ir_remote_reader(
    pin: esp_idf_hal::gpio::Gpio4<'static>,
    lamp_handle: Arc<LampHandle>,
    led_state: Arc<LedState>,
    play_pause_ready: &'static std::sync::atomic::AtomicBool,
) {
    let spawn_result = std::thread::Builder::new()
        .name("irRemote".into())
        .stack_size(4096)
        .spawn(move || {
            let driver = match PinDriver::input(pin, Pull::Up) {
                Ok(d) => d,
                Err(e) => {
                    warn!("[ir] pin init failed: {e}, giving up on this thread");
                    return;
                }
            };
            info!("[ir] remote receiver ready on GPIO4");
            loop {
                if driver.is_low() {
                    match read_nec_frame(&driver) {
                        Ok((_address, command)) => {
                            // Same 80ms red-flash acknowledgment the physical buttons
                            // already give on every press -- fires for any successfully
                            // decoded button, including ones not mapped to an action yet,
                            // so pressing the remote always visibly confirms "the
                            // receiver saw that" rather than only sometimes.
                            led_state.set_red(true);
                            std::thread::sleep(Duration::from_millis(80));
                            led_state.set_red(false);
                            dispatch(command, &lamp_handle, play_pause_ready);
                        }
                        Err(IrError::ChecksumMismatch) => {
                            warn!("[ir] checksum mismatch, dropping frame");
                        }
                        Err(IrError::Timeout) => {
                            // Malformed/partial frame (e.g. a repeat code's shorter
                            // timing) -- not a real button press, ignore.
                        }
                    }
                    // Let the trailing edge fully settle before watching for the next press,
                    // so we don't immediately re-trigger on our own frame's tail.
                    std::thread::sleep(Duration::from_millis(100));
                } else {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        });
    if let Err(e) = spawn_result {
        warn!("[ir] thread spawn failed: {e}");
    }
}
