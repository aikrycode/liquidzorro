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
static PLUGIN: OnceLock<PluginState> = OnceLock::new();
// Zorro wants a 6-digit-minimum trade id.
static NEXT_TRADE_ID: AtomicI32 = AtomicI32::new(100_000);

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
    PLUGIN.get()
}

/// Stores the logged-in clients. `false` if already logged in.
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
