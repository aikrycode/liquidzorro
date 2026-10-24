//! Zorro broker-plugin C ABI exports.
//!
//! Signatures verified against `include\trading.h` and the real
//! `Simulation.cpp` sample plugin from a Zorro S 2026 (build 3.1.1.2)
//! install at `C:\Users\Matusovsky\Zorro2026`. See the plan file for the
//! exact verbatim declarations this was checked against.

use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};

use crate::{bridge, state};

/// Zorro's `DATE`: an OLE automation date (days since 1899-12-30), not
/// defined in trading.h itself — plugins declare it themselves.
pub type Date = f64;

/// Candle with extra data, matching trading.h's `T6` exactly. No explicit
/// `repr(packed)` needed: natural alignment and the lite-C side's
/// `#pragma pack(push,4)` produce an identical 32-byte layout for this
/// particular field set.
#[repr(C)]
pub struct T6 {
    pub time: Date,
    pub f_high: f32,
    pub f_low: f32,
    pub f_open: f32,
    pub f_close: f32,
    pub f_val: f32,
    pub f_vol: f32,
}

const PLUGIN_NAME: &[u8] = b"Hyperliquid\0";
const PLUGIN_VERSION: c_int = 2;

/// Reads a Zorro-supplied `char*` argument as a `&str`, returning `None` for
/// a null pointer or invalid UTF-8.
unsafe fn read_cstr<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

#[no_mangle]
pub unsafe extern "C" fn BrokerOpen(
    name: *mut c_char,
    fp_error: *const c_void,
    fp_progress: *const c_void,
) -> c_int {
    if !name.is_null() {
        std::ptr::copy_nonoverlapping(PLUGIN_NAME.as_ptr(), name as *mut u8, PLUGIN_NAME.len());
    }
    let error_cb = (!fp_error.is_null())
        .then(|| std::mem::transmute::<*const c_void, state::ErrorFn>(fp_error));
    let progress_cb = (!fp_progress.is_null())
        .then(|| std::mem::transmute::<*const c_void, state::ProgressFn>(fp_progress));
    state::set_callbacks(error_cb, progress_cb);
    PLUGIN_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn BrokerLogin(
    user: *mut c_char,
    _pwd: *mut c_char,
    _kind: *mut c_char,
    _accounts: *mut c_char,
) -> c_int {
    // Zorro calls BrokerLogin(NULL, ...) to log out; we hold no per-session
    // resources worth tearing down yet, so just report success.
    if user.is_null() {
        return 1;
    }
    match bridge::login() {
        Ok(()) => 1,
        Err(e) => {
            state::log_error(&format!("Hyperliquid login failed: {e}"));
            0
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn BrokerTime(p_time_gmt: *mut Date) -> c_int {
    if state::get().is_none() {
        return 0;
    }
    if !p_time_gmt.is_null() {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        *p_time_gmt = unix_ms as f64 / 86_400_000.0 + 25569.0;
    }
    2 // online
}

#[no_mangle]
pub unsafe extern "C" fn BrokerAsset(
    asset: *mut c_char,
    p_price: *mut f64,
    p_spread: *mut f64,
    p_volume: *mut f64,
    p_pip: *mut f64,
    p_pip_cost: *mut f64,
    p_min_amount: *mut f64,
    p_margin_cost: *mut f64,
    p_roll_long: *mut f64,
    p_roll_short: *mut f64,
) -> c_int {
    let Some(symbol) = read_cstr(asset) else {
        return 0;
    };
    let Some(price) = bridge::asset_price(symbol) else {
        return 0;
    };

    if !p_price.is_null() {
        *p_price = price;
    }
    // Placeholders until spot_meta_and_asset_contexts() is wired in to supply
    // real tick size / spread; Hyperliquid spot has no margin or rollover.
    if !p_spread.is_null() {
        *p_spread = 0.0;
    }
    if !p_volume.is_null() {
        *p_volume = 0.0;
    }
    if !p_pip.is_null() {
        *p_pip = 1.0;
    }
    if !p_pip_cost.is_null() {
        *p_pip_cost = 1.0;
    }
    if !p_min_amount.is_null() {
        *p_min_amount = 0.0;
    }
    if !p_margin_cost.is_null() {
        *p_margin_cost = 100.0; // spot = fully collateralized, no leverage
    }
    if !p_roll_long.is_null() {
        *p_roll_long = 0.0;
    }
    if !p_roll_short.is_null() {
        *p_roll_short = 0.0;
    }
    1
}

#[no_mangle]
pub unsafe extern "C" fn BrokerHistory2(
    asset: *mut c_char,
    _t_start: Date,
    t_end: Date,
    n_tick_minutes: c_int,
    n_ticks: c_int,
    ticks: *mut T6,
) -> c_int {
    let Some(symbol) = read_cstr(asset) else {
        return 0;
    };
    if ticks.is_null() || n_ticks <= 0 {
        return 0;
    }
    let Some(candles) = bridge::history(symbol, t_end, n_tick_minutes, n_ticks) else {
        return 0;
    };
    for (i, candle) in candles.iter().enumerate() {
        std::ptr::write(
            ticks.add(i),
            T6 {
                time: candle.time,
                f_high: candle.f_high,
                f_low: candle.f_low,
                f_open: candle.f_open,
                f_close: candle.f_close,
                f_val: candle.f_val,
                f_vol: candle.f_vol,
            },
        );
    }
    candles.len() as c_int
}

/// TODO(Phase 3): GET_COMPLIANCE/GET_MAXREQUESTS/GET_BROKERZONE/GET_MAXTICKS/
/// SET_SYMBOL/SET_ORDERTYPE, extended once BrokerBuy2/BrokerTrade land and we
/// can see what a live Zorro64 session actually requests.
#[no_mangle]
pub unsafe extern "C" fn BrokerCommand(_command: c_int, _parameter: isize) -> f64 {
    0.0
}
