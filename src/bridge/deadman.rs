//! Dead-man's switch: if Zorro stops talking to the exchange, the exchange
//! cancels every open order on the account by itself.
//!
//! Why: a resting limit order or stop keeps working after Zorro freezes,
//! crashes, or loses its connection, with nothing left to manage it. Here
//! the plugin keeps pushing a cancel deadline (Hyperliquid's
//! `scheduleCancel`) into the future from `BrokerTime`, which Zorro calls
//! many times a minute. While Zorro runs the deadline never arrives; once
//! the calls stop, it does.
//!
//! Switched on by SET_DEADMAN with a number of seconds, off with 0 or at
//! logout. It cancels orders only: open positions stay open, and so lose
//! any exchange-side stop the plugin rested for them.
//!
//! Hyperliquid's limits: the account must have traded $1,000,000 in all
//! before the exchange accepts a schedule, and the switch may fire at most
//! 10 times a day.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperliquid_rust_sdk::ExchangeResponseStatus;

use crate::{runtime, state};

/// The shortest and longest window accepted. Each renewal is one exchange
/// request, sent every third of the window, so 30s means one every 10s.
pub const MIN_SECONDS: i32 = 30;
pub const MAX_SECONDS: i32 = 86_400;

struct Armed {
    window: Duration,
    renewed: Instant,
}

static ARMED: Mutex<Option<Armed>> = Mutex::new(None);

/// A failing renewal is logged once, not on every `BrokerTime`.
static WARNED: AtomicBool = AtomicBool::new(false);

fn armed() -> std::sync::MutexGuard<'static, Option<Armed>> {
    ARMED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Seconds the switch is set to, 0 when off.
pub fn seconds() -> i32 {
    armed().as_ref().map_or(0, |a| a.window.as_secs() as i32)
}

/// Sets the window to `seconds`, or switches the switch off with 0. Returns
/// the seconds now set; an error leaves the switch as it was.
pub fn set(seconds: i32) -> Result<i32, String> {
    if seconds == 0 {
        off()?;
        return Ok(0);
    }
    if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
        return Err(format!("{seconds}s is outside {MIN_SECONDS}-{MAX_SECONDS}"));
    }
    let window = Duration::from_secs(seconds as u64);
    schedule(Some(deadline(window)))?;
    *armed() = Some(Armed { window, renewed: Instant::now() });
    WARNED.store(false, Ordering::Relaxed);
    Ok(seconds)
}

/// Clears the schedule if one is set. At logout Zorro is leaving on
/// purpose, and its orders are meant to stay.
pub fn off() -> Result<(), String> {
    let mut guard = armed();
    if guard.is_none() {
        return Ok(());
    }
    schedule(None)?;
    *guard = None;
    Ok(())
}

/// Drops the switch without a request: for a new login, whose account may
/// not be the one the schedule was set on.
pub fn forget() {
    *armed() = None;
}

/// Pushes the deadline back when it is due. Called from `BrokerTime`.
pub fn renew() {
    let mut guard = armed();
    let Some(a) = guard.as_mut() else { return };
    if !due(a.window, a.renewed, Instant::now()) {
        return;
    }
    match schedule(Some(deadline(a.window))) {
        Ok(()) => {
            a.renewed = Instant::now();
            if WARNED.swap(false, Ordering::Relaxed) {
                state::log_message("Hyperliquid: dead-man's switch renewed again");
            }
        }
        Err(e) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                state::log_message(&format!(
                    "Hyperliquid: dead-man's switch not renewed ({e}); open orders are cancelled if this persists"
                ));
            }
        }
    }
}

/// Renewing every third of the window leaves two more tries before the
/// deadline if one request fails.
fn due(window: Duration, renewed: Instant, now: Instant) -> bool {
    now.saturating_duration_since(renewed) >= window / 3
}

fn deadline(window: Duration) -> u64 {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    now_ms + window.as_millis() as u64
}

fn schedule(time: Option<u64>) -> Result<(), String> {
    let plugin = state::get().ok_or("not logged in")?;
    let reply = runtime::get()
        .block_on(plugin.exchange_client.schedule_cancel(time, None))
        .map_err(|e| format!("request failed: {e}"))?;
    accepted(reply)
}

fn accepted(reply: ExchangeResponseStatus) -> Result<(), String> {
    match reply {
        ExchangeResponseStatus::Ok(_) => Ok(()),
        ExchangeResponseStatus::Err(e) => Err(format!("refused: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renews_every_third_of_the_window() {
        let start = Instant::now();
        let window = Duration::from_secs(60);
        assert!(!due(window, start, start));
        assert!(!due(window, start, start + Duration::from_secs(19)));
        assert!(due(window, start, start + Duration::from_secs(20)));
        assert!(due(window, start, start + Duration::from_secs(300)));
    }

    /// Out of range is refused before any request, and leaves it off.
    #[test]
    fn refuses_a_window_out_of_range() {
        for s in [1, 29, 86_401, -30] {
            assert!(set(s).is_err(), "{s}s");
        }
        assert_eq!(seconds(), 0);
    }

    /// Off while already off needs no exchange (nor a login).
    #[test]
    fn switching_off_when_off_is_quiet() {
        assert_eq!(set(0), Ok(0));
    }

    #[test]
    fn a_refusal_names_the_reason() {
        let e = "Cannot set scheduled cancel time until enough volume traded.".to_string();
        assert_eq!(accepted(ExchangeResponseStatus::Err(e.clone())), Err(format!("refused: {e}")));
    }
}
