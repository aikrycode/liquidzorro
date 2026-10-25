//! Shared plugin state: the logged-in Hyperliquid clients, open-trade
//! bookkeeping, and the Zorro-supplied message/progress callbacks captured
//! in `BrokerOpen`.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use alloy::primitives::Address;
use hyperliquid_rust_sdk::{ExchangeClient, InfoClient};

pub type ErrorFn = unsafe extern "C" fn(*const c_char) -> c_int;
pub type ProgressFn = unsafe extern "C" fn(c_int) -> c_int;

/// Bookkeeping for one order placed via `BrokerBuy2`, keyed by the Zorro
/// trade id we made up (Hyperliquid's own `oid` isn't guaranteed to fit
/// Zorro's expectations, and Zorro needs a stable id to query back later).
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
static PLUGIN: OnceLock<PluginState> = OnceLock::new();
// Zorro's manual asks for a "6-digit minimum" trade id, so start comfortably above that.
static NEXT_TRADE_ID: AtomicI32 = AtomicI32::new(100_000);

struct Callbacks {
    error: Option<ErrorFn>,
    #[allow(dead_code)] // wired up once BrokerBuy2/BrokerHistory2 report long-running progress
    progress: Option<ProgressFn>,
}

pub fn set_callbacks(error: Option<ErrorFn>, progress: Option<ProgressFn>) {
    let _ = CALLBACKS.set(Callbacks { error, progress });
}

/// Reports a message back to Zorro via the `fpError` callback from `BrokerOpen`,
/// falling back to stderr if the plugin hasn't been opened yet (shouldn't happen
/// in practice, since Zorro always calls BrokerOpen first).
pub fn log_error(msg: &str) {
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
    PLUGIN.get()
}

/// Stores the logged-in clients. Returns `false` if already logged in
/// (Zorro is not expected to call BrokerLogin twice without an intervening
/// logout, so this just guards against double-init rather than replacing).
pub fn init(
    info_client: InfoClient,
    exchange_client: ExchangeClient,
    address: Address,
) -> bool {
    PLUGIN
        .set(PluginState {
            info_client,
            exchange_client,
            address,
            trades: Mutex::new(HashMap::new()),
        })
        .is_ok()
}

/// Records a newly placed order and hands back the Zorro-facing trade id for it.
pub fn register_trade(coin: &str, oid: u64, is_buy: bool, entry_price: f64, entry_sz: f64) -> i32 {
    let id = NEXT_TRADE_ID.fetch_add(1, Ordering::Relaxed);
    if let Some(plugin) = get() {
        // Recover from poisoning rather than cascading a panic: a previous
        // panic while the lock was held shouldn't make every future trade
        // lookup through the FFI boundary panic too.
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
