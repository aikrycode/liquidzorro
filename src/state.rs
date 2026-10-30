//! Shared plugin state: the logged-in Hyperliquid clients, open-trade
//! bookkeeping, and the Zorro-supplied message/progress callbacks captured
//! in `BrokerOpen`.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use alloy::primitives::Address;
use hyperliquid_rust_sdk::{ExchangeClient, InfoClient};

use crate::assets::AssetInfo;

pub type ErrorFn = unsafe extern "C" fn(*const c_char) -> c_int;
pub type ProgressFn = unsafe extern "C" fn(c_int) -> c_int;

/// Bookkeeping for one order, keyed by the Zorro trade id we made up.
#[derive(Clone)]
pub struct TradeInfo {
    pub coin: String,
    pub oid: u64,
    pub is_buy: bool,
    pub entry_price: f64,
    pub entry_sz: f64,
    /// Size still open. Drops as partial closes fill; a trade is only
    /// retired when this reaches zero, not when one close fills.
    pub remaining_sz: f64,
    /// Set once closed; order status alone never reports that.
    pub closed: bool,
}

pub struct PluginState {
    pub info_client: InfoClient,
    pub exchange_client: ExchangeClient,
    pub address: Address,
    trades: Mutex<HashMap<i32, TradeInfo>>,
    /// Symbol -> universe/precision, so an order doesn't pay for a `meta()`
    /// round-trip. Cleared with the session, since it is network-specific.
    assets: Mutex<HashMap<String, AssetInfo>>,
    /// Perp symbol -> leverage Hyperliquid has configured for this account.
    /// Keeps `BrokerAsset` off the network on every call; filled on first
    /// read and kept current by `SET_LEVERAGE`.
    leverage: Mutex<HashMap<String, u32>>,
}

static CALLBACKS: OnceLock<Callbacks> = OnceLock::new();
// Holds a leaked `&'static PluginState` rather than the value itself, so
// `get()` can hand out a plain reference (no lock guard held across the
// `.await`s in bridge) while still allowing logout/re-login to replace it.
// Each login leaks one PluginState; a handful per session is negligible,
// and outstanding references from an in-flight call stay valid.
static PLUGIN: RwLock<Option<&'static PluginState>> = RwLock::new(None);
// Zorro wants a 6-digit-minimum trade id.
static NEXT_TRADE_ID: AtomicI32 = AtomicI32::new(100_000);
// Last SET_ORDERTYPE from Zorro: 0 = FOK, 1 = IOC, 2 = GTC. Defaults to IOC
// so an order placed before Zorro sets one still behaves as a market order.
static ORDER_TYPE: AtomicI32 = AtomicI32::new(1);

pub fn set_order_type(order_type: i32) {
    ORDER_TYPE.store(order_type, Ordering::Relaxed);
}

pub fn order_type() -> i32 {
    ORDER_TYPE.load(Ordering::Relaxed)
}

// Asset named by the last SET_SYMBOL, which scopes commands like
// SET_LEVERAGE that carry no symbol of their own.
static SYMBOL: Mutex<Option<String>> = Mutex::new(None);

pub fn set_symbol(symbol: &str) {
    *SYMBOL.lock().unwrap_or_else(|e| e.into_inner()) = Some(symbol.to_string());
}

pub fn symbol() -> Option<String> {
    SYMBOL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

struct Callbacks {
    error: Option<ErrorFn>,
    #[allow(dead_code)] // wired up once long-running calls report progress
    progress: Option<ProgressFn>,
}

pub fn set_callbacks(error: Option<ErrorFn>, progress: Option<ProgressFn>) {
    let _ = CALLBACKS.set(Callbacks { error, progress });
}

/// Reports a message via the `fpError` callback -- real plugins use this
/// for ordinary status too, not just errors; it's the only channel to
/// Zorro's message window. Falls back to stderr if not yet opened.
pub fn log_message(msg: &str) {
    match CALLBACKS.get().and_then(|c| c.error) {
        Some(cb) => {
            if let Ok(c_msg) = CString::new(msg) {
                unsafe { cb(c_msg.as_ptr()) };
            }
        }
        None => eprintln!("liquidzorro: {msg}"),
    }
}

pub fn get() -> Option<&'static PluginState> {
    *PLUGIN.read().unwrap_or_else(|e| e.into_inner())
}

/// Stores the logged-in clients, replacing any previous session.
pub fn init(info_client: InfoClient, exchange_client: ExchangeClient, address: Address) {
    let state: &'static PluginState = Box::leak(Box::new(PluginState {
        info_client,
        exchange_client,
        address,
        trades: Mutex::new(HashMap::new()),
        assets: Mutex::new(HashMap::new()),
        leverage: Mutex::new(HashMap::new()),
    }));
    *PLUGIN.write().unwrap_or_else(|e| e.into_inner()) = Some(state);
}

/// Drops the current session, so the next `BrokerLogin` reconnects from
/// scratch (with whatever credentials/network Zorro supplies then) rather
/// than silently reusing the old one.
pub fn clear() {
    *PLUGIN.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Records a newly placed order, returns the Zorro-facing trade id.
pub fn register_trade(coin: &str, oid: u64, is_buy: bool, entry_price: f64, entry_sz: f64) -> i32 {
    let id = NEXT_TRADE_ID.fetch_add(1, Ordering::Relaxed);
    if let Some(plugin) = get() {
        // recover from poisoning instead of cascading a panic
        plugin
            .trades
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
            id,
            TradeInfo {
                coin: coin.to_string(),
                oid,
                is_buy,
                entry_price,
                entry_sz,
                remaining_sz: entry_sz,
                closed: false,
            },
        );
    }
    id
}

/// Books `qty` as closed and returns what is still open. The trade is
/// marked closed only once nothing remains -- a partial close that fills
/// its requested amount does not retire the trade.
pub fn reduce_trade(zorro_id: i32, qty: f64) -> f64 {
    let Some(plugin) = get() else { return 0.0 };
    let mut trades = plugin.trades.lock().unwrap_or_else(|e| e.into_inner());
    let Some(info) = trades.get_mut(&zorro_id) else { return 0.0 };
    let (remaining, flat) = apply_close(info.remaining_sz, info.entry_sz, qty);
    info.remaining_sz = remaining;
    info.closed = flat;
    remaining
}

/// `(remaining, is_flat)` after closing `qty` of a trade. Split out so the
/// rule can be tested without a session.
fn apply_close(remaining_sz: f64, entry_sz: f64, qty: f64) -> (f64, bool) {
    let remaining = (remaining_sz - qty).max(0.0);
    // tolerance so float dust can't keep a flat trade open forever
    if remaining <= entry_sz * 1e-6 {
        (0.0, true)
    } else {
        (remaining, false)
    }
}

#[cfg(test)]
mod tests {
    use super::apply_close;

    /// Overfilling one chunk must not retire a trade that still holds more:
    /// doing so made Zorro drop a position with a third of it still open.
    #[test]
    fn a_chunk_filling_in_full_does_not_retire_the_trade() {
        let entry = 0.28625;
        let (rem, flat) = apply_close(entry, entry, 0.11);
        assert!(!flat, "trade retired with {rem} still open");
        let (rem, flat) = apply_close(rem, entry, 0.09); // filled more than asked
        assert!(!flat, "trade retired with {rem} still open");
        assert!((rem - 0.08625).abs() < 1e-9, "expected 0.08625, got {rem}");
    }

    #[test]
    fn closing_everything_retires_the_trade() {
        let entry = 0.28625;
        let (rem, flat) = apply_close(entry, entry, entry);
        assert!(flat);
        assert_eq!(rem, 0.0);
    }

    #[test]
    fn overshoot_clamps_at_zero() {
        let entry = 0.5;
        let (rem, flat) = apply_close(entry, entry, 10.0);
        assert!(flat);
        assert_eq!(rem, 0.0);
    }

    #[test]
    fn float_dust_counts_as_flat() {
        let entry = 1.0;
        let (rem, flat) = apply_close(1e-12, entry, 0.0);
        assert!(flat, "a speck left by rounding should not keep it open");
        assert_eq!(rem, 0.0);
    }
}

pub fn cached_asset(symbol: &str) -> Option<AssetInfo> {
    get()?
        .assets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(symbol)
        .cloned()
}

pub fn cache_asset(symbol: &str, info: AssetInfo) {
    if let Some(plugin) = get() {
        plugin
            .assets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(symbol.to_string(), info);
    }
}

pub fn cached_leverage(symbol: &str) -> Option<u32> {
    get()?
        .leverage
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(symbol)
        .copied()
}

pub fn cache_leverage(symbol: &str, leverage: u32) {
    if let Some(plugin) = get() {
        plugin
            .leverage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(symbol.to_string(), leverage);
    }
}

pub fn get_trade(zorro_id: i32) -> Option<TradeInfo> {
    get()?
        .trades
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&zorro_id)
        .cloned()
}
