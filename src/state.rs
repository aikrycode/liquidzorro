//! Shared plugin state: the logged-in Hyperliquid clients, and the
//! Zorro-supplied message/progress callbacks captured in `BrokerOpen`.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

use hyperliquid_rust_sdk::{ExchangeClient, InfoClient};

pub type ErrorFn = unsafe extern "C" fn(*const c_char) -> c_int;
pub type ProgressFn = unsafe extern "C" fn(c_int) -> c_int;

pub struct PluginState {
    pub info_client: InfoClient,
    pub exchange_client: ExchangeClient,
}

static CALLBACKS: OnceLock<Callbacks> = OnceLock::new();
static PLUGIN: OnceLock<PluginState> = OnceLock::new();

struct Callbacks {
    error: Option<ErrorFn>,
    #[allow(dead_code)] // wired up once BrokerHistory2/BrokerBuy2 report long-running progress
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
pub fn init(state: PluginState) -> bool {
    PLUGIN.set(state).is_ok()
}
