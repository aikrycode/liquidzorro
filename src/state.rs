//! Shared plugin state: Hyperliquid clients and the live asset/price cache.
//!
//! Populated in `BrokerLogin` (Phase 2) once real clients exist; for now it's
//! an empty placeholder so other modules compile against a stable shape.

use std::sync::OnceLock;

pub struct PluginState {
    // TODO(Phase 2): hyperliquid_rust_sdk::ExchangeClient, InfoClient, asset map, trade-id map.
}

static STATE: OnceLock<PluginState> = OnceLock::new();

pub fn get() -> Option<&'static PluginState> {
    STATE.get()
}

pub fn init(state: PluginState) {
    let _ = STATE.set(state);
}
