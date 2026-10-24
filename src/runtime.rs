//! Owns the single Tokio runtime the DLL uses to drive the async Hyperliquid SDK
//! from Zorro's synchronous broker-plugin calls.

use std::sync::OnceLock;
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub fn get() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Runtime::new().expect("failed to start Tokio runtime for liquidzorro")
    })
}
