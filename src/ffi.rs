//! Zorro broker-plugin C ABI exports.
//!
//! Signatures here are best-effort reconstructions from the Zorro manual
//! (https://zorro-project.com/manual/en/brokerplugin.htm). They are STUBS and
//! MUST be re-verified against the real `trading.h`/`variables.h` from a
//! Zorro install's `Source` folder before Phase 2 work proceeds — struct
//! packing and exact field order are not fully documented publicly, and a
//! mismatch here causes silent memory corruption rather than a compile error.

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};

const PLUGIN_NAME: &[u8] = b"Hyperliquid\0";
const INTERFACE_VERSION: c_int = 2;

/// TODO(Phase 2): verify exact signature against trading.h.
#[no_mangle]
pub unsafe extern "C" fn BrokerOpen(
    name: *mut c_char,
    _fp_message: *const c_void,
    _fp_progress: *const c_void,
) -> c_int {
    if !name.is_null() {
        std::ptr::copy_nonoverlapping(PLUGIN_NAME.as_ptr(), name as *mut u8, PLUGIN_NAME.len());
    }
    INTERFACE_VERSION
}

/// TODO(Phase 2): spawn the Tokio runtime, construct InfoClient/ExchangeClient
/// against Hyperliquid testnet, and store them in `state::PluginState`.
#[no_mangle]
pub unsafe extern "C" fn BrokerLogin(
    _user: *mut c_char,
    _pwd: *mut c_char,
    _kind: *mut c_char,
    _accounts: *mut c_char,
) -> c_int {
    // Not logged in yet — stub only, proves the DLL loads and exports resolve.
    0
}
