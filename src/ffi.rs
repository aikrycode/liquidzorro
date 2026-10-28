//! Zorro broker-plugin C ABI exports, verified against a real `trading.h`
//! and `Simulation.cpp` (Zorro S 2026).
//!
//! Every export is wrapped in `guard()`: a panic crossing the FFI boundary
//! is UB, so panics are caught and turned into a safe default return.

use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};
use std::panic::{self, AssertUnwindSafe};

use crate::{bridge, state};

/// Zorro's `DATE`: OLE automation date, not defined in trading.h itself.
pub type Date = f64;

/// trading.h's `T6`. No `repr(packed)` needed -- natural alignment already
/// matches the lite-C side's `pack(4)` for this field set.
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

/// trading.h's `NAY` sentinel ("no value"), used by `BrokerTrade`.
pub const NAY: i32 = -999_997;

/// Reads a Zorro `char*` as `&str`; `None` for null/invalid UTF-8.
unsafe fn read_cstr<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

/// Catches a panic from `f` and returns `default` instead of unwinding.
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
        // NULL user = logout. Drop the session so the next login actually
        // reconnects instead of silently reusing these clients -- otherwise
        // switching Demo/Real or pasting a different key mid-session would
        // keep talking to the old one.
        if user.is_null() {
            state::clear();
            state::log_message("Hyperliquid: logged out");
            return 1;
        }
        // Password field = private key; Real/Demo toggle = mainnet/testnet.
        let privkey = read_cstr(pwd);
        let is_mainnet = matches!(read_cstr(kind), Some(k) if k.eq_ignore_ascii_case("real"));
        let base_url = if is_mainnet {
            hyperliquid_rust_sdk::BaseUrl::Mainnet
        } else {
            hyperliquid_rust_sdk::BaseUrl::Testnet // default: never land on mainnet by accident
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
            state::log_message(&format!("BrokerAsset({symbol}): price unavailable"));
            return 0;
        };

        if !p_price.is_null() {
            *p_price = price;
        }
        // Placeholders until spot_meta_and_asset_contexts() is wired in.
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
            // Margin required per lot. Spot is fully collateralized; a perp
            // needs price/leverage, so Zorro sizes leveraged positions right
            // instead of assuming they cost their full notional.
            *p_margin_cost = bridge::margin_cost_per_lot(symbol, price);
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
            state::log_message(&format!("BrokerHistory2({symbol}): history unavailable"));
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

/// Positive `Amount` buys, negative sells -- no separate close export.
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
        let Some(symbol) = read_cstr(asset) else {
            state::log_message("BrokerBuy2: invalid/missing asset name");
            return 0;
        };
        // Zorro signals "this call closes a position" with StopDist == -1;
        // the sign of Amount alone can't distinguish closing a long from
        // opening a short. Any other value is a stop distance, still TODO.
        let closing = stop_dist == -1.0;
        let side = match (amount > 0, closing) {
            (true, false) => "buy",
            (false, false) => "sell",
            (true, true) => "close short",
            (false, true) => "close long",
        };
        match bridge::place_order(symbol, amount, limit, closing) {
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

/// GET_COMPLIANCE: NFA-restriction query. `2` means no NFA restrictions,
/// matching the sample plugins; a lower value makes Zorro refuse to close
/// trades with "Error 075".
const GET_COMPLIANCE: c_int = 51;

/// GET_POSITION: real held balance for the symbol in `parameter` (`char*`),
/// read from Hyperliquid directly rather than Zorro's local (process-only)
/// trade bookkeeping -- survives a full restart. AMOUNT_SCALE sub-units.
const GET_POSITION: c_int = 53;

/// GET_BROKERZONE: time zone of the broker's timestamps. `0` = UTC, which
/// is what Hyperliquid returns.
const GET_BROKERZONE: c_int = 40;

/// GET_MAXTICKS: cap on ticks per `BrokerHistory2` call. Hyperliquid's
/// `candles_snapshot` tops out at 5000 per request.
const GET_MAXTICKS: c_int = 43;

/// SET_ORDERTYPE: 0 = fill-or-kill, 1 = immediate-or-cancel, 2 = GTC.
/// Echoing the value back is how a plugin tells Zorro it honours order
/// types at all -- returning 0 makes Zorro assume limit orders are
/// unsupported and skip them entirely.
const SET_ORDERTYPE: c_int = 157;

/// DO_CANCEL: cancel the resting order with the Zorro trade id in
/// `parameter`.
const DO_CANCEL: c_int = 301;

/// SET_SYMBOL: the asset subsequent commands apply to.
const SET_SYMBOL: c_int = 132;

/// SET_LEVERAGE: leverage for subsequent perp trades on the SET_SYMBOL asset.
const SET_LEVERAGE: c_int = 140;

/// Unhandled codes return 0, which Zorro reads as "unsupported".
#[no_mangle]
pub unsafe extern "C" fn BrokerCommand(command: c_int, parameter: isize) -> f64 {
    guard("BrokerCommand", 0.0, || match command {
        GET_COMPLIANCE => 2.0,
        GET_BROKERZONE => 0.0, // UTC
        GET_MAXTICKS => 5000.0,
        SET_ORDERTYPE => {
            let order_type = parameter as i32;
            state::set_order_type(order_type);
            order_type as f64
        }
        SET_SYMBOL => match read_cstr(parameter as *const c_char) {
            Some(symbol) => {
                state::set_symbol(symbol);
                1.0
            }
            None => 0.0,
        },
        SET_LEVERAGE => {
            // applies to whichever asset SET_SYMBOL last named
            let Some(symbol) = state::symbol() else {
                state::log_message("BrokerCommand(SET_LEVERAGE): no symbol set");
                return 0.0;
            };
            let leverage = parameter as i32;
            match bridge::set_leverage(&symbol, leverage.max(0) as u32) {
                Ok(()) => {
                    state::log_message(&format!(
                        "BrokerCommand(SET_LEVERAGE): {symbol} set to {leverage}x cross"
                    ));
                    leverage as f64
                }
                Err(e) => {
                    state::log_message(&format!("BrokerCommand(SET_LEVERAGE, {symbol}) FAILED: {e}"));
                    0.0
                }
            }
        }
        DO_CANCEL => {
            let trade_id = parameter as i32;
            match bridge::cancel_order(trade_id) {
                Ok(()) => {
                    state::log_message(&format!("BrokerCommand(DO_CANCEL): cancelled {trade_id}"));
                    1.0
                }
                Err(e) => {
                    state::log_message(&format!(
                        "BrokerCommand(DO_CANCEL, {trade_id}) FAILED: {e}"
                    ));
                    0.0
                }
            }
        }
        GET_POSITION => {
            let Some(symbol) = read_cstr(parameter as *const c_char) else {
                return 0.0;
            };
            match bridge::net_position(symbol) {
                Some(qty) => qty * bridge::AMOUNT_SCALE,
                None => {
                    state::log_message(&format!(
                        "BrokerCommand(GET_POSITION, {symbol}): unavailable (not logged in?)"
                    ));
                    0.0
                }
            }
        }
        _ => 0.0,
    })
}
