//! Zorro broker-plugin C ABI exports.
//!
//! Signatures verified against `include\trading.h` and the real
//! `Simulation.cpp` sample plugin from a Zorro S 2026 (build 3.1.1.2)
//! install at `C:\Users\Matusovsky\Zorro2026`. See the plan file for the
//! exact verbatim declarations this was checked against.
//!
//! Every exported function is wrapped in `guard()`: an unwinding Rust panic
//! that reached across this `extern "C"` boundary would be undefined
//! behavior inside Zorro's process, so any panic here is caught and turned
//! into an ordinary failure return instead of risking a crash of the whole
//! host application.

use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};
use std::panic::{self, AssertUnwindSafe};

use crate::{bridge, state};

/// Zorro's `DATE`: an OLE automation date (days since 1899-12-30), not
/// defined in trading.h itself — plugins declare it themselves.
pub type Date = f64;

/// Candle with extra data, matching trading.h's `T6` exactly. No explicit
/// `repr(packed)` needed: natural alignment and the lite-C side's
/// `#pragma pack(push,4)` produce an identical 32-byte layout for this
/// particular field set.
#[repr(C)]
#[derive(Debug)]
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

/// trading.h's `#define NAY -999997` sentinel, used by `BrokerTrade` for
/// "no value"/"not available" style returns.
pub const NAY: i32 = -999_997;

/// Reads a Zorro-supplied `char*` argument as a `&str`, returning `None` for
/// a null pointer or invalid UTF-8.
unsafe fn read_cstr<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

/// Runs `f`, catching any panic so it can't unwind across the FFI boundary.
/// On panic, logs via `state::log_message` and returns `default` instead.
fn guard<R>(label: &str, default: R, f: impl FnOnce() -> R) -> R {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            state::log_message(&format!("liquidzorro: panic in {label}, returning a safe default"));
            default
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn BrokerOpen(
    name: *mut c_char,
    fp_error: *const c_void,
    fp_progress: *const c_void,
) -> c_int {
    guard("BrokerOpen", 0, || {
        if !name.is_null() {
            std::ptr::copy_nonoverlapping(PLUGIN_NAME.as_ptr(), name as *mut u8, PLUGIN_NAME.len());
        }
        let error_cb = (!fp_error.is_null())
            .then(|| std::mem::transmute::<*const c_void, state::ErrorFn>(fp_error));
        let progress_cb = (!fp_progress.is_null())
            .then(|| std::mem::transmute::<*const c_void, state::ProgressFn>(fp_progress));
        state::set_callbacks(error_cb, progress_cb);
        state::log_message("Hyperliquid plugin loaded (liquidzorro)");
        PLUGIN_VERSION
    })
}

#[no_mangle]
pub unsafe extern "C" fn BrokerLogin(
    user: *mut c_char,
    pwd: *mut c_char,
    kind: *mut c_char,
    _accounts: *mut c_char,
) -> c_int {
    guard("BrokerLogin", 0, || {
        // Zorro calls BrokerLogin(NULL, ...) to log out; we hold no per-session
        // resources worth tearing down yet, so just report success.
        if user.is_null() {
            state::log_message("Hyperliquid: logout requested");
            return 1;
        }
        // Zorro's Password field holds the Hyperliquid private key (there's
        // no separate "API key" concept for a raw EVM wallet, so the User
        // field goes unused); its Real/Demo toggle selects mainnet/testnet.
        let privkey = read_cstr(pwd);
        let is_mainnet = matches!(read_cstr(kind), Some(k) if k.eq_ignore_ascii_case("real"));
        let base_url = if is_mainnet {
            hyperliquid_rust_sdk::BaseUrl::Mainnet
        } else {
            // Default to testnet (including an unrecognized/missing Type)
            // so an ambiguous login can never accidentally hit mainnet.
            hyperliquid_rust_sdk::BaseUrl::Testnet
        };
        state::log_message(&format!(
            "Hyperliquid: logging in ({})...",
            if is_mainnet { "mainnet" } else { "testnet" }
        ));
        match bridge::login(privkey, base_url) {
            Ok(address) => {
                state::log_message(&format!(
                    "Hyperliquid: login OK ({}), address={address}",
                    if is_mainnet { "mainnet" } else { "testnet" }
                ));
                1
            }
            Err(e) => {
                state::log_message(&format!("Hyperliquid: login FAILED: {e}"));
                0
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn BrokerTime(p_time_gmt: *mut Date) -> c_int {
    guard("BrokerTime", 0, || {
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
    })
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
    guard("BrokerAsset", 0, || {
        let Some(symbol) = read_cstr(asset) else {
            state::log_message("BrokerAsset: invalid/missing asset name");
            return 0;
        };
        if state::get().is_none() {
            state::log_message(&format!("BrokerAsset({symbol}): not logged in"));
            return 0;
        }
        let Some(price) = bridge::asset_price(symbol) else {
            state::log_message(&format!(
                "BrokerAsset({symbol}): price unavailable -- check the symbol matches Hyperliquid's spot_meta() exactly"
            ));
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
    })
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
    guard("BrokerHistory2", 0, || {
        let Some(symbol) = read_cstr(asset) else {
            state::log_message("BrokerHistory2: invalid/missing asset name");
            return 0;
        };
        if ticks.is_null() || n_ticks <= 0 {
            return 0;
        }
        let Some(candles) = bridge::history(symbol, t_end, n_tick_minutes, n_ticks) else {
            state::log_message(&format!(
                "BrokerHistory2({symbol}): candle history unavailable (not logged in, or bad symbol)"
            ));
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
    })
}

#[no_mangle]
pub unsafe extern "C" fn BrokerAccount(
    _account: *mut c_char,
    p_balance: *mut f64,
    p_trade_val: *mut f64,
    p_margin_val: *mut f64,
) -> c_int {
    guard("BrokerAccount", 0, || {
        let Some((balance, trade_val, margin_val)) = bridge::account_balance() else {
            state::log_message("BrokerAccount: balance unavailable (not logged in?)");
            return 0;
        };
        if !p_balance.is_null() {
            *p_balance = balance;
        }
        if !p_trade_val.is_null() {
            *p_trade_val = trade_val;
        }
        if !p_margin_val.is_null() {
            *p_margin_val = margin_val;
        }
        1
    })
}

/// Positive `Amount` buys, negative sells/closes -- Zorro's ABI has no
/// separate "sell"/"close" export, so the sign of `Amount` is the only
/// signal `BrokerBuy2` gets.
#[no_mangle]
pub unsafe extern "C" fn BrokerBuy2(
    asset: *mut c_char,
    amount: c_int,
    stop_dist: f64,
    limit: f64,
    p_price: *mut f64,
    p_fill: *mut c_int,
) -> c_int {
    guard("BrokerBuy2", 0, || {
        let _ = stop_dist; // TODO(Phase 4): wire to a Hyperliquid trigger order
        let Some(symbol) = read_cstr(asset) else {
            state::log_message("BrokerBuy2: invalid/missing asset name");
            return 0;
        };
        let side = if amount > 0 { "buy" } else { "sell" };
        match bridge::place_order(symbol, amount, limit) {
            Ok((zorro_id, price, fill)) => {
                if !p_price.is_null() {
                    *p_price = price;
                }
                if !p_fill.is_null() {
                    *p_fill = fill;
                }
                state::log_message(&format!(
                    "BrokerBuy2: {side} {symbol} OK, filled={fill} @ {price}, trade id {zorro_id}"
                ));
                zorro_id
            }
            Err(e) => {
                state::log_message(&format!("BrokerBuy2: {side} {symbol} FAILED: {e}"));
                0
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn BrokerTrade(
    trade_id: c_int,
    p_open: *mut f64,
    p_close: *mut f64,
    p_cost: *mut f64,
    p_profit: *mut f64,
) -> c_int {
    guard("BrokerTrade", NAY, || {
        let Some((open, close, cost, profit, fill)) = bridge::trade_status(trade_id) else {
            state::log_message(&format!("BrokerTrade({trade_id}): unknown trade id"));
            return NAY;
        };
        if !p_open.is_null() {
            *p_open = open;
        }
        if !p_close.is_null() {
            *p_close = close;
        }
        if !p_cost.is_null() {
            *p_cost = cost;
        }
        if !p_profit.is_null() {
            *p_profit = profit;
        }
        fill
    })
}

/// TODO(Phase 4): GET_COMPLIANCE/GET_MAXREQUESTS/GET_BROKERZONE/GET_MAXTICKS/
/// SET_SYMBOL/SET_ORDERTYPE, extended once we can see what a live Zorro64
/// session actually requests.
#[no_mangle]
pub unsafe extern "C" fn BrokerCommand(_command: c_int, _parameter: isize) -> f64 {
    guard("BrokerCommand", 0.0, || 0.0)
}
