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
}

pub struct PluginState {
    pub info_client: InfoClient,
    pub exchange_client: ExchangeClient,
    pub address: Address,
    trades: Mutex<HashMap<i32, TradeInfo>>,
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
            },
        );
    }
    id
}

pub fn get_trade(zorro_id: i32) -> Option<TradeInfo> {
    get()?
        .trades
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&zorro_id)
        .cloned()
}
