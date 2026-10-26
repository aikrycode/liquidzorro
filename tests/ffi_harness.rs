//! Drives the compiled `liquidzorro.dll` the way Zorro will: loaded
//! dynamically, each export called by its real C signature with real raw
//! pointers and callbacks -- exercises the actual FFI boundary, not just
//! our Rust functions directly.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use libloading::{Library, Symbol};

type BrokerOpenFn = unsafe extern "C" fn(*mut c_char, *const c_void, *const c_void) -> c_int;
type BrokerLoginFn =
    unsafe extern "C" fn(*mut c_char, *mut c_char, *mut c_char, *mut c_char) -> c_int;
type BrokerTimeFn = unsafe extern "C" fn(*mut f64) -> c_int;
#[allow(clippy::type_complexity)]
type BrokerAssetFn = unsafe extern "C" fn(
    *mut c_char,
    *mut f64,
    *mut f64,
    *mut f64,
    *mut f64,
    *mut f64,
    *mut f64,
    *mut f64,
    *mut f64,
    *mut f64,
) -> c_int;
type BrokerHistory2Fn =
    unsafe extern "C" fn(*mut c_char, f64, f64, c_int, c_int, *mut T6Mirror) -> c_int;
type BrokerAccountFn = unsafe extern "C" fn(*mut c_char, *mut f64, *mut f64, *mut f64) -> c_int;
type BrokerBuy2Fn =
    unsafe extern "C" fn(*mut c_char, c_int, f64, f64, *mut f64, *mut c_int) -> c_int;
type BrokerTradeFn = unsafe extern "C" fn(c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> c_int;
type BrokerCommandFn = unsafe extern "C" fn(c_int, isize) -> f64;

/// Mirrors `ffi::T6`'s layout without depending on the lib crate.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct T6Mirror {
    time: f64,
    f_high: f32,
    f_low: f32,
    f_open: f32,
    f_close: f32,
    f_val: f32,
    f_vol: f32,
}

static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

unsafe extern "C" fn test_error_cb(msg: *const c_char) -> c_int {
    if !msg.is_null() {
        if let Ok(s) = CStr::from_ptr(msg).to_str() {
            *LAST_ERROR.lock().unwrap() = Some(s.to_string());
        }
    }
    0
}

unsafe extern "C" fn test_progress_cb(_pct: c_int) -> c_int {
    0
}

fn take_last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap().take()
}

/// Locates the cdylib built alongside this test binary: test binaries land
/// in `target/<profile>/deps/`, and the cdylib sits one directory up.
fn dylib_path() -> std::path::PathBuf {
    let mut exe = std::env::current_exe().expect("current_exe");
    exe.pop(); // deps/
    exe.pop(); // <profile>/
    exe.join("liquidzorro.dll")
}

fn cstring_buf(min_len: usize) -> Vec<c_char> {
    vec![0 as c_char; min_len]
}

/// Exercises every export via its real C signature, network-free
/// (HL_TESTNET_PRIVKEY is unset, so BrokerLogin fails fast locally).
#[test]
fn drives_every_export_like_zorro_would() {
    std::env::remove_var("HL_TESTNET_PRIVKEY");

    let path = dylib_path();
    let lib = unsafe { Library::new(&path) }
        .unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));

    unsafe {
        let broker_open: Symbol<BrokerOpenFn> = lib.get(b"BrokerOpen\0").unwrap();
        let broker_login: Symbol<BrokerLoginFn> = lib.get(b"BrokerLogin\0").unwrap();
        let broker_time: Symbol<BrokerTimeFn> = lib.get(b"BrokerTime\0").unwrap();
        let broker_asset: Symbol<BrokerAssetFn> = lib.get(b"BrokerAsset\0").unwrap();
        let broker_history2: Symbol<BrokerHistory2Fn> = lib.get(b"BrokerHistory2\0").unwrap();
        let broker_account: Symbol<BrokerAccountFn> = lib.get(b"BrokerAccount\0").unwrap();
        let broker_buy2: Symbol<BrokerBuy2Fn> = lib.get(b"BrokerBuy2\0").unwrap();
        let broker_trade: Symbol<BrokerTradeFn> = lib.get(b"BrokerTrade\0").unwrap();
        let broker_command: Symbol<BrokerCommandFn> = lib.get(b"BrokerCommand\0").unwrap();

        // BrokerOpen: real callback function pointers, real output buffer.
        let mut name_buf = cstring_buf(32);
        let version = broker_open(
            name_buf.as_mut_ptr(),
            test_error_cb as *const c_void,
            test_progress_cb as *const c_void,
        );
        assert_eq!(version, 2, "expected PLUGIN_VERSION 2");
        let name = CStr::from_ptr(name_buf.as_ptr()).to_str().unwrap();
        assert_eq!(name, "Hyperliquid");

        // BrokerLogin with no HL_TESTNET_PRIVKEY set: fails fast, no network,
        // and the failure message should reach us through the real callback.
        let user = CString::new("anyuser").unwrap();
        let login_result = broker_login(
            user.as_ptr() as *mut c_char,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_eq!(login_result, 0, "login should fail without a key");
        let err = take_last_error().expect("expected an error callback message");
        assert!(
            err.contains("HL_TESTNET_PRIVKEY"),
            "unexpected error message: {err}"
        );

        // Everything below requires being logged in, which we deliberately
        // are not -- confirms the "not logged in" path is reached correctly
        // through real pointers, and that output buffers are left untouched.
        let asset = CString::new("HYPE/USDC").unwrap();

        let mut time_sentinel = f64::MIN;
        let rc = broker_time(&mut time_sentinel as *mut f64);
        assert_eq!(rc, 0);
        assert_eq!(time_sentinel, f64::MIN, "BrokerTime wrote despite not being logged in");

        let (mut price, mut spread, mut volume, mut pip, mut pip_cost) =
            (f64::MIN, f64::MIN, f64::MIN, f64::MIN, f64::MIN);
        let (mut min_amount, mut margin_cost, mut roll_long, mut roll_short) =
            (f64::MIN, f64::MIN, f64::MIN, f64::MIN);
        let rc = broker_asset(
            asset.as_ptr() as *mut c_char,
            &mut price,
            &mut spread,
            &mut volume,
            &mut pip,
            &mut pip_cost,
            &mut min_amount,
            &mut margin_cost,
            &mut roll_long,
            &mut roll_short,
        );
        assert_eq!(rc, 0);
        assert_eq!(price, f64::MIN, "BrokerAsset wrote despite not being logged in");

        let sentinel_tick = T6Mirror {
            time: f64::MIN,
            f_high: f32::MIN,
            f_low: f32::MIN,
            f_open: f32::MIN,
            f_close: f32::MIN,
            f_val: f32::MIN,
            f_vol: f32::MIN,
        };
        let mut ticks = vec![sentinel_tick; 5];
        let rc = broker_history2(
            asset.as_ptr() as *mut c_char,
            0.0,
            0.0,
            60,
            5,
            ticks.as_mut_ptr(),
        );
        assert_eq!(rc, 0);
        assert!(
            ticks.iter().all(|t| *t == sentinel_tick),
            "BrokerHistory2 wrote despite not being logged in"
        );

        let (mut balance, mut trade_val, mut margin_val) = (f64::MIN, f64::MIN, f64::MIN);
        let rc = broker_account(
            std::ptr::null_mut(),
            &mut balance,
            &mut trade_val,
            &mut margin_val,
        );
        assert_eq!(rc, 0);

        let (mut buy_price, mut buy_fill) = (f64::MIN, i32::MIN);
        let rc = broker_buy2(
            asset.as_ptr() as *mut c_char,
            1,
            0.0,
            0.0,
            &mut buy_price,
            &mut buy_fill,
        );
        assert_eq!(rc, 0, "BrokerBuy2 should fail without being logged in");
        let err = take_last_error().expect("expected a BrokerBuy2 failure message");
        assert!(err.contains("BrokerBuy2") && err.contains("FAILED"), "unexpected message: {err}");

        let (mut open, mut close, mut cost, mut profit) = (f64::MIN, f64::MIN, f64::MIN, f64::MIN);
        let rc = broker_trade(100_000, &mut open, &mut close, &mut cost, &mut profit);
        assert_eq!(rc, -999_997, "expected the NAY sentinel for an unknown trade id");

        let rc = broker_command(0, 0);
        assert_eq!(rc, 0.0);
    }
}
