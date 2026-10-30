//! Sync-to-async bridge: each call does a short `block_on` over a REST
//! request against the hyperliquid_rust_sdk clients.
//!
//! TODO: BrokerAsset does one REST round-trip per call; move to a cached
//! WS `AllMids` subscription once polling performance needs it.

use std::env;

use alloy::signers::local::PrivateKeySigner;
use hyperliquid_rust_sdk::{
    BaseUrl, CandlesSnapshotResponse, ClientCancelRequest, ClientLimit, ClientOrder,
    ClientOrderRequest, ExchangeClient, ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};

use crate::assets::{self, AssetInfo};
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

/// Zorro's SET_ORDERTYPE value for a good-till-cancelled (resting) order.
pub const ORDER_TYPE_GTC: i32 = 2;

/// Market order if `limit == 0.0`, else a limit order that rests (GTC) or
/// fills immediately depending on Zorro's SET_ORDERTYPE. Positive `amount`
/// buys, negative sells -- there is no separate close export.
///
/// `closing` is Zorro's `StopDist == -1` flag, meaning "this call is closing
/// a position". On perps that becomes `reduce_only`, so an oversized close
/// flattens instead of silently flipping into an opposing position. Spot has
/// no position to reduce, so the flag is inert there.
///
/// Returns `(zorro_trade_id, fill_price, fill_amount)` in `AMOUNT_SCALE` units.
///
/// TODO: a real `stop_dist` isn't wired to a Hyperliquid trigger order yet.
pub fn place_order(
    symbol: &str,
    amount: i32,
    limit: f64,
    closing: bool,
) -> Result<(i32, f64, i32), String> {
    let plugin = state::get().ok_or("not logged in")?;
    if amount == 0 {
        return Err("amount must be nonzero".to_string());
    }
    let is_buy = amount > 0;
    let raw_sz = amount.unsigned_abs() as f64 / AMOUNT_SCALE;

    let (sz, limit_px, response) = runtime::get().block_on(async {
        // Rounding rules depend on which universe the symbol lives in --
        // notably prices, where perps allow fewer decimals than spot.
        let asset = resolve_asset(&plugin.info_client, symbol).await?;
        let sz = assets::round_size(raw_sz, asset.sz_decimals);

        // SDK's market_open() only checks the perp universe for price, 404s
        // on spot pairs -- build the order ourselves so one path serves both.
        let (limit_px, tif) = if limit > 0.0 {
            // Hyperliquid has no fill-or-kill TIF, so FOK degrades to IOC --
            // immediate like FOK, but may fill partially instead of killing.
            let tif = if state::order_type() == ORDER_TYPE_GTC { "Gtc" } else { "Ioc" };
            // An unrounded price straight from Zorro is rejected as invalid.
            (assets::round_price(limit, &asset), tif)
        } else {
            (
                marketable_price(&plugin.info_client, symbol, &asset, is_buy, 0.05).await?,
                "Ioc",
            )
        };

        let order = ClientOrderRequest {
            asset: symbol.to_string(),
            is_buy,
            reduce_only: closing && asset.is_perp(),
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
        Ok::<_, String>((sz, limit_px, response))
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
            let price: f64 = order.avg_px.parse().unwrap_or(limit_px);
            let fill_sz: f64 = order.total_sz.parse().unwrap_or(sz);
            let zorro_id = state::register_trade(symbol, order.oid, is_buy, price, fill_sz);
            Ok((zorro_id, price, (fill_sz * AMOUNT_SCALE).round() as i32))
        }
        // Resting: accepted but unfilled. Zorro reads fill 0 as a pending
        // trade and polls BrokerTrade until it fills or it cancels it.
        ExchangeDataStatus::Resting(order) => {
            let zorro_id = state::register_trade(symbol, order.oid, is_buy, limit_px, sz);
            Ok((zorro_id, limit_px, 0))
        }
        ExchangeDataStatus::Error(e) => Err(format!("order error: {e}")),
        other => Err(format!("unexpected order status: {other:?}")),
    }
}

/// Resolves `symbol` to its universe and precision, caching the result --
/// otherwise every order pays for a `meta()`/`spot_meta()` round-trip.
pub(crate) async fn resolve_asset(info: &InfoClient, symbol: &str) -> Result<AssetInfo, String> {
    if let Some(cached) = state::cached_asset(symbol) {
        return Ok(cached);
    }
    let resolved = assets::fetch(info, symbol).await?;
    state::cache_asset(symbol, resolved.clone());
    Ok(resolved)
}

/// Slippage-adjusted price for an IOC "market" order, rounded to whatever
/// the asset's universe allows.
async fn marketable_price(
    info: &InfoClient,
    symbol: &str,
    asset: &AssetInfo,
    is_buy: bool,
    slippage: f64,
) -> Result<f64, String> {
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
    Ok(assets::round_price(slipped, asset))
}

/// Cancels a resting order by its Zorro trade id. Backs DO_CANCEL.
pub fn cancel_order(zorro_id: i32) -> Result<(), String> {
    let plugin = state::get().ok_or("not logged in")?;
    let info = state::get_trade(zorro_id).ok_or_else(|| format!("unknown trade id {zorro_id}"))?;

    let response = runtime::get()
        .block_on(plugin.exchange_client.cancel(
            ClientCancelRequest {
                asset: info.coin.clone(),
                oid: info.oid,
            },
            None,
        ))
        .map_err(|e| format!("cancel request failed: {e}"))?;

    match response {
        ExchangeResponseStatus::Err(e) => Err(format!("cancel rejected: {e}")),
        ExchangeResponseStatus::Ok(r) => match r.data.and_then(|d| d.statuses.into_iter().next()) {
            Some(ExchangeDataStatus::Error(e)) => Err(format!("cancel error: {e}")),
            // Success comes back as a bare "success" status; anything else
            // that isn't an explicit error means the order is no longer open,
            // which is what the caller wanted either way.
            _ => Ok(()),
        },
    }
}

/// Status of a tracked order: `(open_price, close_price, cost, profit,
/// fill_state)`; `fill_state` follows BrokerTrade's contract.
pub fn trade_status(zorro_id: i32) -> Option<(f64, f64, f64, f64, i32)> {
    let plugin = state::get()?;
    let info = state::get_trade(zorro_id)?;

    // -1 = completely closed. Hyperliquid reports a filled order as "filled"
    // forever, so the flag is the only way Zorro learns the trade is gone.
    if info.closed {
        return Some((info.entry_price, info.entry_price, 0.0, 0.0, -1));
    }

    let status = runtime::get()
        .block_on(plugin.info_client.query_order_by_oid(plugin.address, info.oid))
        .ok()?;

    let close_price = asset_price(&info.coin).unwrap_or(info.entry_price);
    let direction = if info.is_buy { 1.0 } else { -1.0 };

    // fill_state is in AMOUNT_SCALE sub-units, like BrokerBuy2's fill_amount.
    // filled_sz drives profit: an order still resting has none, so it must
    // not accrue P&L just because the market moved away from its limit.
    let (fill_state, filled_sz) = match status.order {
        None => (-1, info.entry_sz), // no longer tracked -> treat as closed
        Some(order_info) => match order_info.status.as_str() {
            // Report what is still open, not the original fill: partial
            // closes shrink the trade and Zorro tracks it by this number.
            "filled" => (
                (info.remaining_sz * AMOUNT_SCALE).round() as i32,
                info.remaining_sz,
            ),
            "canceled" | "rejected" | "marginCanceled" => (NAY - 1, 0.0),
            _ => {
                let remaining: f64 = order_info.order.sz.parse().unwrap_or(info.entry_sz);
                let filled = (info.entry_sz - remaining).max(0.0);
                ((filled * AMOUNT_SCALE).round() as i32, filled)
            }
        },
    };
    let profit = direction * (close_price - info.entry_price) * filled_sz;

    Some((info.entry_price, close_price, 0.0, profit, fill_state))
}

/// `(balance, trade_val, margin_val)` for `BrokerAccount`.
///
/// The perp account is the meaningful one once positions exist: account
/// value already includes unrealized P&L, and margin in use is what limits
/// further trading. Spot USDC sitting outside the perp wallet is added on
/// so a spot-only account still reports a balance.
pub fn account_balance() -> Option<(f64, f64, f64)> {
    let plugin = state::get()?;
    let (state_resp, token_resp) = runtime::get().block_on(async {
        let a = plugin.info_client.user_state(plugin.address).await;
        let b = plugin.info_client.user_token_balances(plugin.address).await;
        (a, b)
    });

    let spot_usdc = token_resp
        .ok()
        .and_then(|r| {
            r.balances
                .iter()
                .find(|b| b.coin == "USDC")
                .and_then(|b| b.total.parse::<f64>().ok())
        })
        .unwrap_or(0.0);

    let Ok(user_state) = state_resp else {
        // perp account unreadable -- a spot-only balance is still useful
        return Some((spot_usdc, 0.0, 0.0));
    };
    let account_value: f64 = user_state.margin_summary.account_value.parse().unwrap_or(0.0);
    let margin_used: f64 = user_state
        .margin_summary
        .total_margin_used
        .parse()
        .unwrap_or(0.0);
    let unrealized: f64 = user_state
        .asset_positions
        .iter()
        .filter_map(|p| p.position.unrealized_pnl.parse::<f64>().ok())
        .sum();

    Some((account_value + spot_usdc, unrealized, margin_used))
}

/// Net position for `symbol`, read from Hyperliquid rather than Zorro's
/// process-local bookkeeping, so it survives a restart. Backs GET_POSITION.
///
/// **Signed**: a perp short is negative. Spot holdings cannot go below zero.
/// `0.0` (not `None`) means "no position".
pub fn net_position(symbol: &str) -> Option<f64> {
    let plugin = state::get()?;
    let asset = runtime::get()
        .block_on(resolve_asset(&plugin.info_client, symbol))
        .ok()?;

    if asset.is_perp() {
        let user_state = runtime::get()
            .block_on(plugin.info_client.user_state(plugin.address))
            .ok()?;
        return Some(
            user_state
                .asset_positions
                .iter()
                .find(|p| p.position.coin == symbol)
                .and_then(|p| p.position.szi.parse::<f64>().ok())
                .unwrap_or(0.0),
        );
    }

    // spot: the held quantity of the pair's base token
    let spot_meta = runtime::get().block_on(plugin.info_client.spot_meta()).ok()?;
    let pair = spot_meta.universe.iter().find(|a| a.name == symbol)?;
    let base_token = spot_meta.tokens.iter().find(|t| t.index == pair.tokens[0])?;
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

/// Margin a single lot ties up, for `BrokerAsset`. Spot posts the full
/// notional; a perp posts notional divided by its leverage.
pub fn margin_cost_per_lot(symbol: &str, price: f64) -> f64 {
    let notional_per_lot = price / AMOUNT_SCALE;
    let Some(plugin) = state::get() else {
        return notional_per_lot;
    };
    let Ok(asset) = runtime::get().block_on(resolve_asset(&plugin.info_client, symbol)) else {
        return notional_per_lot;
    };
    if !asset.is_perp() {
        return notional_per_lot;
    }
    let leverage = current_leverage(symbol).unwrap_or(1).max(1);
    notional_per_lot / leverage as f64
}

/// Leverage Hyperliquid has configured for this account on `symbol`.
///
/// Read from `active_asset_data`, which reports the setting whether or not
/// a position is open -- the clearinghouse state lists only open positions,
/// so sizing the first entry after a leverage change would use the default.
/// Cached, because `BrokerAsset` asks on every call.
fn current_leverage(symbol: &str) -> Option<u32> {
    if let Some(cached) = state::cached_leverage(symbol) {
        return Some(cached);
    }
    let plugin = state::get()?;
    let data = runtime::get()
        .block_on(
            plugin
                .info_client
                .active_asset_data(plugin.address, symbol.to_string()),
        )
        .ok()?;
    state::cache_leverage(symbol, data.leverage.value);
    Some(data.leverage.value)
}

/// Sets leverage for a perp. Backs Zorro's SET_LEVERAGE.
pub fn set_leverage(symbol: &str, leverage: u32) -> Result<(), String> {
    let plugin = state::get().ok_or("not logged in")?;
    if leverage == 0 {
        return Err("leverage must be at least 1".to_string());
    }
    let response = runtime::get().block_on(async {
        let asset = resolve_asset(&plugin.info_client, symbol).await?;
        // Zorro sends this before every order, spot included. Spot is
        // always 1x, so accept that quietly and only object to real
        // leverage being asked of an unleveraged asset.
        if !asset.is_perp() {
            return if leverage <= 1 {
                Ok(None)
            } else {
                Err(format!("{symbol} is spot and cannot use {leverage}x"))
            };
        }
        if leverage > asset.max_leverage {
            return Err(format!(
                "leverage {leverage} exceeds the maximum of {} for {symbol}",
                asset.max_leverage
            ));
        }
        plugin
            .exchange_client
            .update_leverage(leverage, symbol, true, None) // cross margin
            .await
            .map(Some)
            .map_err(|e| format!("update_leverage failed: {e}"))
    })?;

    match response {
        None => Ok(()), // spot: nothing to set
        Some(ExchangeResponseStatus::Err(e)) => Err(format!("leverage rejected: {e}")),
        Some(ExchangeResponseStatus::Ok(_)) => {
            state::cache_leverage(symbol, leverage);
            Ok(())
        }
    }
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
