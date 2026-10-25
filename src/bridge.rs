//! Sync-to-async bridge between Zorro's blocking FFI calls and the async
//! hyperliquid_rust_sdk clients. Each call here does a short `block_on` over
//! a REST request.
//!
//! TODO: BrokerAsset currently does one `all_mids` REST round-trip per call.
//! Zorro polls BrokerAsset per-asset at TickTime intervals, so once this is
//! tested against a live Zorro session this should move to a WS `AllMids`
//! subscription feeding a mutex-protected cache instead, so polling never
//! blocks on the network.

use std::env;

use alloy::signers::local::PrivateKeySigner;
use hyperliquid_rust_sdk::{BaseUrl, CandlesSnapshotResponse, ExchangeClient, InfoClient};

use crate::ffi::T6;
use crate::{runtime, state};

const TESTNET_PRIVKEY_ENV: &str = "HL_TESTNET_PRIVKEY";

/// Logs into Hyperliquid testnet using the private key from `HL_TESTNET_PRIVKEY`
/// and stores the resulting clients in `state`. Returns an error description
/// on failure (bad/missing key, unreachable API, etc.) for `BrokerLogin` to
/// report via `state::log_error`.
pub fn login() -> Result<(), String> {
    let privkey = env::var(TESTNET_PRIVKEY_ENV)
        .map_err(|_| format!("{TESTNET_PRIVKEY_ENV} is not set"))?;
    let wallet: PrivateKeySigner = privkey
        .parse()
        .map_err(|e| format!("invalid private key: {e}"))?;

    let (info_client, exchange_client) = runtime::get().block_on(async {
        let info_client = InfoClient::new(None, Some(BaseUrl::Testnet))
            .await
            .map_err(|e| format!("InfoClient connect failed: {e}"))?;
        let exchange_client = ExchangeClient::new(None, wallet, Some(BaseUrl::Testnet), None, None)
            .await
            .map_err(|e| format!("ExchangeClient connect failed: {e}"))?;
        // Sanity-check connectivity before reporting a successful login.
        info_client
            .spot_meta()
            .await
            .map_err(|e| format!("spot_meta probe failed: {e}"))?;
        Ok::<_, String>((info_client, exchange_client))
    })?;

    if !state::init(state::PluginState {
        info_client,
        exchange_client,
    }) {
        return Err("already logged in".to_string());
    }
    Ok(())
}

/// Last ask price for `symbol` (a Hyperliquid spot coin name, e.g. "PURR/USDC"),
/// via a REST `all_mids` call.
pub fn asset_price(symbol: &str) -> Option<f64> {
    let plugin = state::get()?;
    let mids = runtime::get()
        .block_on(plugin.info_client.all_mids())
        .ok()?;
    mids.get(symbol)?.parse().ok()
}

/// Historical candles for `symbol`, most recent first, matching Zorro's
/// BrokerHistory2 contract. `n_tick_minutes` is mapped to the closest
/// Hyperliquid candle interval Hyperliquid supports.
pub fn history(symbol: &str, end_ole: f64, n_tick_minutes: i32, n_ticks: i32) -> Option<Vec<T6>> {
    let plugin = state::get()?;
    let interval = interval_for_minutes(n_tick_minutes);
    let end_ms = ole_to_unix_millis(end_ole);
    let span_ms = n_ticks as i64 * n_tick_minutes as i64 * 60_000;
    let start_ms = (end_ms - span_ms).max(0);

    let mut candles = runtime::get()
        .block_on(
            plugin
                .info_client
                .candles_snapshot(symbol.to_string(), interval.to_string(), start_ms as u64, end_ms as u64),
        )
        .ok()?;

    // Hyperliquid returns candles oldest-first; Zorro expects newest-first.
    candles.sort_by_key(|c| std::cmp::Reverse(c.time_open));
    candles.truncate(n_ticks.max(0) as usize);
    Some(candles.into_iter().map(candle_to_t6).collect())
}

fn candle_to_t6(c: CandlesSnapshotResponse) -> T6 {
    let parse = |s: &str| s.parse::<f32>().unwrap_or(0.0);
    T6 {
        time: unix_millis_to_ole(c.time_close as i64),
        f_high: parse(&c.high),
        f_low: parse(&c.low),
        f_open: parse(&c.open),
        f_close: parse(&c.close),
        f_val: 0.0,
        f_vol: parse(&c.vlm),
    }
}

/// Picks the Hyperliquid candle interval closest to (but not finer than)
/// Zorro's requested bar length.
fn interval_for_minutes(minutes: i32) -> &'static str {
    const STEPS: &[(i32, &str)] = &[
        (1, "1m"),
        (3, "3m"),
        (5, "5m"),
        (15, "15m"),
        (30, "30m"),
        (60, "1h"),
        (120, "2h"),
        (240, "4h"),
        (480, "8h"),
        (720, "12h"),
        (1440, "1d"),
    ];
    STEPS
        .iter()
        .rev()
        .find(|(step, _)| minutes >= *step)
        .map(|(_, name)| *name)
        .unwrap_or("1m")
}

/// OLE Automation date -> Unix epoch milliseconds. OLE day 0 is 1899-12-30;
/// Unix epoch (1970-01-01) is 25569 OLE days later.
fn ole_to_unix_millis(ole: f64) -> i64 {
    ((ole - 25569.0) * 86_400_000.0).round() as i64
}

fn unix_millis_to_ole(unix_millis: i64) -> f64 {
    unix_millis as f64 / 86_400_000.0 + 25569.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live Phase 2 smoke test against Hyperliquid testnet. Ignored by
    /// default since it needs network + HL_TESTNET_PRIVKEY; run explicitly
    /// with `cargo test -- --ignored --nocapture`. Everything it calls
    /// (login/spot_meta/all_mids/candles_snapshot) is a read-only info
    /// endpoint -- no orders are placed, so it never spends testnet USDC.
    #[test]
    #[ignore]
    fn login_and_query_testnet() {
        login().expect("login should succeed");
        let plugin = state::get().expect("state should be initialized after login");

        let spot_meta = runtime::get()
            .block_on(plugin.info_client.spot_meta())
            .expect("spot_meta should succeed");
        let pair = spot_meta
            .universe
            .first()
            .expect("testnet should have at least one spot pair")
            .name
            .clone();
        println!("first spot pair from spot_meta universe: {pair}");

        let price = asset_price(&pair);
        println!("{pair} price via all_mids: {price:?}");
        assert!(price.is_some(), "expected a price for {pair} from all_mids");

        let now_ole = unix_millis_to_ole(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
        );
        let candles = history(&pair, now_ole, 60, 5);
        println!("{pair} last 5 hourly candles: {candles:?}");
        assert!(candles.is_some(), "expected candle history for {pair}");
    }
}
