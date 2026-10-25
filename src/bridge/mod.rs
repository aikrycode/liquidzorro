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
use hyperliquid_rust_sdk::{
    BaseUrl, CandlesSnapshotResponse, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};

use crate::ffi::{T6, NAY};
use crate::{runtime, state};

const TESTNET_PRIVKEY_ENV: &str = "HL_TESTNET_PRIVKEY";

/// Logs into Hyperliquid using `privkey` (from Zorro's Password field --
/// falling back to `HL_TESTNET_PRIVKEY` only if that field is empty, for our
/// own test convenience) against `base_url` (selected by Zorro's Real/Demo
/// toggle: Demo -> testnet, Real -> mainnet). Stores the resulting clients
/// in `state`, and returns the wallet address on success so `BrokerLogin`
/// can report it via `state::log_message`.
pub fn login(privkey: Option<&str>, base_url: BaseUrl) -> Result<alloy::primitives::Address, String> {
    let privkey = match privkey {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => env::var(TESTNET_PRIVKEY_ENV)
            .map_err(|_| "no password supplied and HL_TESTNET_PRIVKEY is not set".to_string())?,
    };
    let wallet: PrivateKeySigner = privkey
        .parse()
        .map_err(|e| format!("invalid private key: {e}"))?;
    let address = wallet.address();

    let (info_client, exchange_client) = runtime::get().block_on(async {
        let info_client = InfoClient::new(None, Some(base_url))
            .await
            .map_err(|e| format!("InfoClient connect failed: {e}"))?;
        let exchange_client = ExchangeClient::new(None, wallet, Some(base_url), None, None)
            .await
            .map_err(|e| format!("ExchangeClient connect failed: {e}"))?;
        // Sanity-check connectivity before reporting a successful login.
        info_client
            .spot_meta()
            .await
            .map_err(|e| format!("spot_meta probe failed: {e}"))?;
        Ok::<_, String>((info_client, exchange_client))
    })?;

    if !state::init(info_client, exchange_client, address) {
        return Err("already logged in".to_string());
    }
    Ok(address)
}

/// `BrokerBuy2`'s `Amount` is a plain C `int`, so it can't represent a
/// fractional base-asset quantity directly -- useless for anything priced
/// like BTC, where even $25 is a tiny fraction of 1 BTC and would round to
/// 0. Instead, `Amount` here counts whole units of `1 / AMOUNT_SCALE` of
/// the base asset (0.00001 by default), and the companion Zorro script
/// declares `vLotAmount = 1` in `assetAdd` so `Lots` passes straight
/// through as this same sub-unit count.
pub const AMOUNT_SCALE: f64 = 100_000.0;

/// Places a market order if `limit` is `0.0`, otherwise a resting (GTC) limit
/// order. `amount > 0` buys, `amount < 0` sells -- there is no separate close
/// export in Zorro's ABI, so `BrokerBuy2`'s sign is the only signal we get.
/// `amount` and the returned fill amount are in `AMOUNT_SCALE` sub-units;
/// returns `(zorro_trade_id, fill_price, fill_amount)`.
///
/// TODO: `stop_dist` (StopDist) isn't wired to a Hyperliquid trigger order
/// yet; Zorro can still manage the stop client-side by calling BrokerBuy2
/// again with a negative amount, so this isn't blocking for v1.
pub fn place_order(symbol: &str, amount: i32, limit: f64) -> Result<(i32, f64, i32), String> {
    let plugin = state::get().ok_or("not logged in")?;
    if amount == 0 {
        return Err("amount must be nonzero".to_string());
    }
    let is_buy = amount > 0;
    let sz = amount.unsigned_abs() as f64 / AMOUNT_SCALE;

    let response = runtime::get().block_on(async {
        // The SDK's own `market_open` helper only looks up sz_decimals/mid
        // price in the perp universe (`meta()`), so it 404s with
        // "Asset not found" for any spot pair -- build the IOC limit order
        // against spot_meta() ourselves instead for a "market" order.
        let (limit_px, tif) = if limit > 0.0 {
            (limit, "Gtc")
        } else {
            (
                marketable_spot_price(&plugin.info_client, symbol, is_buy, 0.05).await?,
                "Ioc",
            )
        };

        let order = ClientOrderRequest {
            asset: symbol.to_string(),
            is_buy,
            reduce_only: false,
            limit_px,
            sz,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: tif.to_string(),
            }),
        };
        plugin
            .exchange_client
            .order(order, None)
            .await
            .map_err(|e| format!("order request failed: {e}"))
    })?;

    let exchange_response = match response {
        ExchangeResponseStatus::Err(e) => return Err(format!("order rejected: {e}")),
        ExchangeResponseStatus::Ok(r) => r,
    };
    let status = exchange_response
        .data
        .and_then(|d| d.statuses.into_iter().next())
        .ok_or("exchange response carried no order status")?;

    match status {
        ExchangeDataStatus::Filled(order) => {
            let price: f64 = order.avg_px.parse().unwrap_or(limit.max(0.0));
            let fill_sz: f64 = order.total_sz.parse().unwrap_or(sz);
            let zorro_id = state::register_trade(symbol, order.oid, is_buy, price, fill_sz);
            Ok((zorro_id, price, (fill_sz * AMOUNT_SCALE).round() as i32))
        }
        ExchangeDataStatus::Resting(order) => {
            let zorro_id = state::register_trade(symbol, order.oid, is_buy, limit, sz);
            Ok((zorro_id, limit, 0))
        }
        ExchangeDataStatus::Error(e) => Err(format!("order error: {e}")),
        other => Err(format!("unexpected order status: {other:?}")),
    }
}

/// Slippage-adjusted limit price for an IOC "market" order on a spot pair,
/// mirroring the private rounding rule `ExchangeClient::market_open` uses
/// internally (5 significant figures, then `8 - base_token.sz_decimals`
/// decimal places) -- but looked up against `spot_meta()` so it actually
/// works for spot pairs instead of only perps.
async fn marketable_spot_price(
    info: &InfoClient,
    symbol: &str,
    is_buy: bool,
    slippage: f64,
) -> Result<f64, String> {
    let spot_meta = info
        .spot_meta()
        .await
        .map_err(|e| format!("spot_meta failed: {e}"))?;
    let asset = spot_meta
        .universe
        .iter()
        .find(|a| a.name == symbol)
        .ok_or_else(|| format!("{symbol} not found in spot_meta universe"))?;
    let base_token = spot_meta
        .tokens
        .iter()
        .find(|t| t.index == asset.tokens[0])
        .ok_or_else(|| format!("base token for {symbol} not found"))?;
    let price_decimals = 8u32.saturating_sub(base_token.sz_decimals as u32);

    let mids = info
        .all_mids()
        .await
        .map_err(|e| format!("all_mids failed: {e}"))?;
    let mid: f64 = mids
        .get(symbol)
        .ok_or_else(|| format!("no mid price for {symbol}"))?
        .parse()
        .map_err(|_| format!("bad mid price for {symbol}"))?;

    let slipped = mid * if is_buy { 1.0 + slippage } else { 1.0 - slippage };
    Ok(round_to_significant_and_decimal(slipped, 5, price_decimals))
}

fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn round_to_significant_and_decimal(value: f64, sig_figs: u32, max_decimals: u32) -> f64 {
    let abs_value = value.abs();
    let magnitude = abs_value.log10().floor() as i32;
    let scale = 10f64.powi(sig_figs as i32 - magnitude - 1);
    let rounded = (abs_value * scale).round() / scale;
    round_to_decimals(rounded.copysign(value), max_decimals)
}

/// Polls Hyperliquid for the current state of a previously placed order.
/// Returns `(open_price, close_price, cost, profit, fill_state)` where
/// `fill_state` follows Zorro's BrokerTrade contract (filled lots, or the
/// `-1`/`NAY-1` sentinels for closed/cancelled).
pub fn trade_status(zorro_id: i32) -> Option<(f64, f64, f64, f64, i32)> {
    let plugin = state::get()?;
    let info = state::get_trade(zorro_id)?;
    let status = runtime::get()
        .block_on(plugin.info_client.query_order_by_oid(plugin.address, info.oid))
        .ok()?;

    let close_price = asset_price(&info.coin).unwrap_or(info.entry_price);
    let direction = if info.is_buy { 1.0 } else { -1.0 };
    let profit = direction * (close_price - info.entry_price) * info.entry_sz;

    // fill_state crosses back into Zorro's Amount domain, so it's in
    // AMOUNT_SCALE sub-units like BrokerBuy2's fill_amount, not real
    // base-asset quantity.
    let fill_state = match status.order {
        None => -1, // Hyperliquid no longer tracks this order -> treat as closed
        Some(order_info) => match order_info.status.as_str() {
            "filled" => (info.entry_sz * AMOUNT_SCALE).round() as i32,
            "canceled" | "rejected" | "marginCanceled" => NAY - 1,
            _ => {
                let remaining: f64 = order_info.order.sz.parse().unwrap_or(info.entry_sz);
                ((info.entry_sz - remaining).max(0.0) * AMOUNT_SCALE).round() as i32
            }
        },
    };

    Some((info.entry_price, close_price, 0.0, profit, fill_state))
}

/// `(balance, trade_val, margin_val)` for `BrokerAccount`, sourced from the
/// USDC row of `user_token_balances`. Spot has no margin, so `margin_val` is
/// always `0.0`; `trade_val` is USDC currently locked in resting orders.
pub fn account_balance() -> Option<(f64, f64, f64)> {
    let plugin = state::get()?;
    let resp = runtime::get()
        .block_on(plugin.info_client.user_token_balances(plugin.address))
        .ok()?;
    let usdc = resp.balances.iter().find(|b| b.coin == "USDC")?;
    let total: f64 = usdc.total.parse().ok()?;
    let hold: f64 = usdc.hold.parse().unwrap_or(0.0);
    Some((total, hold, 0.0))
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
mod tests;
