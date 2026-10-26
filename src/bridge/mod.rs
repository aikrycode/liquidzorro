//! Sync-to-async bridge: each call does a short `block_on` over a REST
//! request against the hyperliquid_rust_sdk clients.
//!
//! TODO: BrokerAsset does one REST round-trip per call; move to a cached
//! WS `AllMids` subscription once polling performance needs it.

use std::env;

use alloy::signers::local::PrivateKeySigner;
use hyperliquid_rust_sdk::{
    BaseUrl, CandlesSnapshotResponse, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};

use crate::ffi::{T6, NAY};
use crate::{runtime, state};

const TESTNET_PRIVKEY_ENV: &str = "HL_TESTNET_PRIVKEY";

/// Logs in with `privkey` (falls back to `HL_TESTNET_PRIVKEY` for dev use)
/// against `base_url`. Replaces any previous session, so a `[Stop]` /
/// `[Trade]` cycle within one Zorro process reconnects with whatever
/// credentials and network are set at that point.
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
        // sanity-check connectivity
        info_client
            .spot_meta()
            .await
            .map_err(|e| format!("spot_meta probe failed: {e}"))?;
        Ok::<_, String>((info_client, exchange_client))
    })?;

    state::init(info_client, exchange_client, address);
    Ok(address)
}

/// `Amount` is a plain C `int`, so it can't hold a fraction of a base-asset
/// unit (e.g. BTC). Instead it counts `1/AMOUNT_SCALE`-sized sub-units; the
/// Zorro script mirrors this via its own AMOUNT_SCALE constant.
pub const AMOUNT_SCALE: f64 = 100_000.0;

/// Market order if `limit == 0.0`, else a resting GTC limit order. Positive
/// `amount` buys, negative sells (no separate close export). Returns
/// `(zorro_trade_id, fill_price, fill_amount)`, all in `AMOUNT_SCALE` units.
///
/// TODO: `stop_dist` isn't wired to a Hyperliquid trigger order yet.
pub fn place_order(symbol: &str, amount: i32, limit: f64) -> Result<(i32, f64, i32), String> {
    let plugin = state::get().ok_or("not logged in")?;
    if amount == 0 {
        return Err("amount must be nonzero".to_string());
    }
    let is_buy = amount > 0;
    let raw_sz = amount.unsigned_abs() as f64 / AMOUNT_SCALE;

    let (sz, response) = runtime::get().block_on(async {
        // round to the asset's real size precision, or Hyperliquid rejects
        // the order ("Order has invalid size")
        let sz_decimals = spot_sz_decimals(&plugin.info_client, symbol).await?;
        let sz = round_to_decimals(raw_sz, sz_decimals);

        // SDK's market_open() only checks the perp universe for price, 404s
        // on spot pairs -- build the IOC limit order against spot_meta() ourselves
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
        let response = plugin
            .exchange_client
            .order(order, None)
            .await
            .map_err(|e| format!("order request failed: {e}"))?;
        Ok::<_, String>((sz, response))
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

/// Decimal places Hyperliquid allows for order sizes on `symbol`'s base asset.
async fn spot_sz_decimals(info: &InfoClient, symbol: &str) -> Result<u32, String> {
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
    Ok(base_token.sz_decimals as u32)
}

/// Slippage-adjusted limit price for an IOC "market" order, rounded to 5
/// significant figures / `8 - sz_decimals` decimals (mirrors the SDK's
/// private rule, but via `spot_meta()` so it works for spot pairs too).
async fn marketable_spot_price(
    info: &InfoClient,
    symbol: &str,
    is_buy: bool,
    slippage: f64,
) -> Result<f64, String> {
    let price_decimals = 8u32.saturating_sub(spot_sz_decimals(info, symbol).await?);

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

/// Status of a previously placed order: `(open_price, close_price, cost,
/// profit, fill_state)`; `fill_state` follows BrokerTrade's contract.
pub fn trade_status(zorro_id: i32) -> Option<(f64, f64, f64, f64, i32)> {
    let plugin = state::get()?;
    let info = state::get_trade(zorro_id)?;
    let status = runtime::get()
        .block_on(plugin.info_client.query_order_by_oid(plugin.address, info.oid))
        .ok()?;

    let close_price = asset_price(&info.coin).unwrap_or(info.entry_price);
    let direction = if info.is_buy { 1.0 } else { -1.0 };
    let profit = direction * (close_price - info.entry_price) * info.entry_sz;

    // fill_state is in AMOUNT_SCALE sub-units, like BrokerBuy2's fill_amount
    let fill_state = match status.order {
        None => -1, // no longer tracked -> treat as closed
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

/// `(balance, trade_val, margin_val)` for `BrokerAccount`. Spot has no
/// margin; `trade_val` is USDC locked in resting orders.
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

/// Actual held base-asset quantity for `symbol`, from Hyperliquid directly
/// (not Zorro's process-local trade bookkeeping). Backs GET_POSITION.
/// `0.0` (not `None`) for "no position".
pub fn net_position(symbol: &str) -> Option<f64> {
    let plugin = state::get()?;
    let spot_meta = runtime::get().block_on(plugin.info_client.spot_meta()).ok()?;
    let asset = spot_meta.universe.iter().find(|a| a.name == symbol)?;
    let base_token = spot_meta
        .tokens
        .iter()
        .find(|t| t.index == asset.tokens[0])?;
    let balances = runtime::get()
        .block_on(plugin.info_client.user_token_balances(plugin.address))
        .ok()?;
    Some(
        balances
            .balances
            .iter()
            .find(|b| b.coin == base_token.name)
            .and_then(|b| b.total.parse::<f64>().ok())
            .unwrap_or(0.0),
    )
}

/// Last ask price for `symbol` via `all_mids`.
pub fn asset_price(symbol: &str) -> Option<f64> {
    let plugin = state::get()?;
    let mids = runtime::get()
        .block_on(plugin.info_client.all_mids())
        .ok()?;
    mids.get(symbol)?.parse().ok()
}

/// Historical candles for `symbol`, newest first (BrokerHistory2 contract).
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

    candles.sort_by_key(|c| std::cmp::Reverse(c.time_open)); // oldest-first -> newest-first
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

/// Closest Hyperliquid candle interval not finer than the requested bar length.
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

/// OLE date -> Unix ms (OLE day 0 = 1899-12-30; epoch is 25569 days later).
fn ole_to_unix_millis(ole: f64) -> i64 {
    ((ole - 25569.0) * 86_400_000.0).round() as i64
}

fn unix_millis_to_ole(unix_millis: i64) -> f64 {
    unix_millis as f64 / 86_400_000.0 + 25569.0
}

#[cfg(test)]
mod tests;
