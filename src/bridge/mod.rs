//! Sync-to-async bridge: each call does a short `block_on` over a REST
//! request against the hyperliquid_rust_sdk clients.
//!
//! TODO: BrokerAsset does one REST round-trip per call; move to a cached
//! WS `AllMids` subscription once polling performance needs it.

use std::env;

use alloy::signers::local::PrivateKeySigner;
use hyperliquid_rust_sdk::{
    BaseUrl, CandlesSnapshotResponse, ClientCancelRequest, ClientLimit, ClientModifyRequest, ClientOrder,
    ClientOrderRequest, ClientTrigger, ExchangeClient, ExchangeDataStatus,
    ExchangeResponseStatus, InfoClient,
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
    // Live prices are an optimisation: without them every price comes over
    // HTTP as before, so a failed feed is logged, not a failed login.
    if let Err(e) = crate::feed::start(base_url) {
        state::log_message(&format!("Hyperliquid: {e}; prices will be polled instead"));
    }
    Ok(address)
}

/// `Amount` is a plain C `int`, so it can't hold a fraction of a base-asset
/// unit (e.g. BTC). Instead it counts `1/AMOUNT_SCALE`-sized sub-units; the
/// Zorro script mirrors this via its own AMOUNT_SCALE constant.
pub const AMOUNT_SCALE: f64 = 100_000.0;

/// Zorro's SET_ORDERTYPE value for a good-till-cancelled (resting) order.
pub const ORDER_TYPE_GTC: i32 = 2;

/// Plugin-specific SET_ORDERTYPE value: good-till-cancelled and post-only,
/// Hyperliquid's "Alo" (add liquidity only). Zorro defines only 0-2; 4
/// keeps clear of the flag bits some plugins add to those.
pub const ORDER_TYPE_POST_ONLY: i32 = 4;

/// Hyperliquid time-in-force for an order under `order_type`.
///
/// Hyperliquid has no fill-or-kill, so FOK degrades to IOC: immediate like
/// FOK, but it may fill partially instead of killing. A market order is
/// always IOC, except post-only: an order that may only rest needs a price
/// to rest at, and quietly sending IOC would take liquidity, the one thing
/// post-only was asked never to do.
fn time_in_force(order_type: i32, has_limit: bool) -> Result<&'static str, String> {
    match (order_type, has_limit) {
        (ORDER_TYPE_POST_ONLY, false) => {
            Err("a post-only order needs a limit price; it cannot be sent at market".to_string())
        }
        (ORDER_TYPE_POST_ONLY, true) => Ok("Alo"),
        (ORDER_TYPE_GTC, true) => Ok("Gtc"),
        _ => Ok("Ioc"),
    }
}

#[cfg(test)]
mod tif_tests {
    use super::*;

    #[test]
    fn market_orders_are_ioc() {
        for order_type in [0, 1, ORDER_TYPE_GTC] {
            assert_eq!(time_in_force(order_type, false), Ok("Ioc"));
        }
    }

    #[test]
    fn limit_orders_follow_the_order_type() {
        assert_eq!(time_in_force(0, true), Ok("Ioc"), "no FOK on Hyperliquid");
        assert_eq!(time_in_force(1, true), Ok("Ioc"));
        assert_eq!(time_in_force(ORDER_TYPE_GTC, true), Ok("Gtc"));
        assert_eq!(time_in_force(ORDER_TYPE_POST_ONLY, true), Ok("Alo"));
    }

    /// Sent as IOC, it would cross the book and pay the taker fee.
    #[test]
    fn a_post_only_market_order_is_refused() {
        assert!(time_in_force(ORDER_TYPE_POST_ONLY, false).is_err());
    }
}

/// Market order if `limit == 0.0`, else a limit order that rests (GTC) or
/// fills immediately depending on Zorro's SET_ORDERTYPE. Positive `amount`
/// buys, negative sells -- there is no separate close export.
///
/// `closing` is Zorro's `StopDist == -1` flag, meaning "this call is closing
/// a position". On perps that becomes `reduce_only`, so an oversized close
/// flattens instead of silently flipping into an opposing position. Spot has
/// no position to reduce, so the flag is inert there.
///
/// `stop_dist` is Zorro's stop-loss distance from the fill price, in price
/// units. Above zero it rests a stop on the exchange once the entry fills,
/// so the position stays protected even with Zorro shut down.
///
/// Returns `(zorro_trade_id, fill_price, fill_amount)` in `AMOUNT_SCALE` units.
pub fn place_order(
    symbol: &str,
    amount: i32,
    limit: f64,
    closing: bool,
    stop_dist: f64,
) -> Result<(i32, f64, i32), String> {
    let plugin = state::get().ok_or("not logged in")?;
    if amount == 0 {
        return Err("amount must be nonzero".to_string());
    }
    let is_buy = amount > 0;
    let raw_sz = amount.unsigned_abs() as f64 / AMOUNT_SCALE;

    // Closes never go out as a TWAP: they fall through and fill now.
    if twap::minutes() > 0 && !closing {
        if stop_dist > 0.0 {
            return Err("a TWAP entry cannot carry a stop: it fills over minutes, and a stop \
needs the filled size; place the stop once the TWAP is done".to_string());
        }
        if !twap::entry_is_tracked(state::order_type()) {
            return Err("a TWAP entry needs setf(TradeMode, TR_GTC): it starts with nothing \
filled, and Zorro only keeps polling a GTC order, so it would never see the slices fill"
                .to_string());
        }
        return twap::place(symbol, is_buy, raw_sz);
    }

    let (sz, limit_px, is_perp, response) = runtime::get().block_on(async {
        // Rounding rules depend on which universe the symbol lives in --
        // notably prices, where perps allow fewer decimals than spot.
        let asset = resolve_asset(&plugin.info_client, symbol).await?;
        // Sizes that reduce a holding are floored, never rounded up. Spot has
        // no reduce-only flag, so asking for more than the balance holds is
        // rejected outright with "Insufficient spot balance" and the position
        // is left open. That is routine, not an edge case: spot fees come out
        // of the base asset, so buying 3 PURR leaves 2.9979 to sell, and
        // 2.9979 rounds up to 3.
        //
        // Keyed on the order itself rather than on `closing`, because a spot
        // sell reduces the balance whether or not the caller said so -- a
        // safety rule that depends on the caller passing the right flag is
        // not a safety rule. Perp closes are covered by `reduce_only`, but
        // flooring them too costs nothing.
        let reduces_a_holding = closing || (!asset.is_perp() && !is_buy);
        let sz = if reduces_a_holding {
            assets::floor_size(raw_sz, asset.sz_decimals)
        } else {
            assets::round_size(raw_sz, asset.sz_decimals)
        };
        // Rounding an order smaller than one size step leaves nothing to
        // send, and the exchange answers only "Order has zero size", which
        // says nothing about why. Name the step and what it costs instead:
        // a market that has run up can price a whole budget below one step.
        if sz <= 0.0 {
            let step = 10f64.powi(-(asset.sz_decimals as i32));
            return Err(format!(
                "{raw_sz} {symbol} rounds to zero at {} decimals; one step is {step} (about {:.2} USDC at the current price)",
                asset.sz_decimals,
                step * mid_price(&plugin.info_client, symbol).await.unwrap_or(0.0)
            ));
        }

        // SDK's market_open() only checks the perp universe for price, 404s
        // on spot pairs -- build the order ourselves so one path serves both.
        // A close is never post-only: it has to happen, and one refused for
        // crossing the book would leave the position open. It keeps the
        // rest of the setting, so a limit close still rests.
        let order_type = match state::order_type() {
            ORDER_TYPE_POST_ONLY if closing => ORDER_TYPE_GTC,
            other => other,
        };
        let tif = time_in_force(order_type, limit > 0.0)?;
        let limit_px = if limit > 0.0 {
            // An unrounded price straight from Zorro is rejected as invalid.
            assets::round_price(limit, &asset)
        } else {
            marketable_price(&plugin.info_client, symbol, &asset, is_buy, 0.05).await?
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
        Ok::<_, String>((sz, limit_px, asset.is_perp(), response))
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
            let gross_sz: f64 = order.total_sz.parse().unwrap_or(sz);
            // A spot buy pays its fee in the coin bought, so the account
            // receives less than the order filled: 10 PURR bought is 9.993
            // held. Zorro must track the 9.993, or the last 0.007 of every
            // trade is a close that can never fill.
            let spot_buy = is_buy && !is_perp;
            let fees = if spot_buy { settled_fees(order.oid) } else { order_fees(order.oid) };
            let fill_sz = gross_sz - fees.map_or(0.0, |f| f.in_base);
            if spot_buy && fees.is_none() {
                state::log_message(&format!(
                    "{symbol} buy {}: fee not indexed yet, tracking the gross {gross_sz}",
                    order.oid
                ));
            }
            let zorro_id = state::register_trade(symbol, order.oid, is_buy, price, gross_sz);
            if let Some(fees) = fees {
                state::book_entry_fees(zorro_id, fees.usd, fees.in_base, true);
            }
            // A stop only means something once there is a fill to protect.
            if stop_dist > 0.0 {
                match place_stop(symbol, is_buy, fill_sz, price, stop_dist) {
                    Ok(stop_oid) => {
                        state::set_stop_oid(zorro_id, stop_oid);
                        state::log_message(&format!(
                            "stop for trade {zorro_id}: {symbol} {stop_dist} away from {price}"
                        ));
                    }
                    // The entry is already on the exchange, so refusing to
                    // report it would lose the position. Say so loudly
                    // instead: the trade is live and unprotected.
                    Err(e) => {
                        state::log_message(&format!("!! trade {zorro_id} has NO STOP: {e}"))
                    }
                }
            }
            Ok((zorro_id, price, (fill_sz * AMOUNT_SCALE).round() as i32))
        }
        // Resting: accepted but unfilled. Zorro reads fill 0 as a pending
        // trade and polls BrokerTrade until it fills or it cancels it.
        ExchangeDataStatus::Resting(order) => {
            let zorro_id = state::register_trade(symbol, order.oid, is_buy, limit_px, sz);
            state::set_pending(zorro_id, true);
            Ok((zorro_id, limit_px, 0))
        }
        // Post-only refusals read "Post only order would have immediately
        // matched"; name the cause so the log says what to change.
        ExchangeDataStatus::Error(e) if e.to_lowercase().contains("post only") => {
            Err(format!("post-only order refused, it would have crossed the book: {e}"))
        }
        ExchangeDataStatus::Error(e) => Err(format!("order error: {e}")),
        other => Err(format!("unexpected order status: {other:?}")),
    }
}

/// Moves a resting order to `new_limit`, and to `new_amount` lots if that is
/// above zero, in one step. Backs the plugin's `DO_MODIFY` command.
///
/// One modify instead of cancel-and-replace: there is no moment with no
/// order on the book, and it is one signed request instead of two. Only an
/// order with nothing filled can be modified. A partial fill has already
/// become a position, and resizing what is left of it would leave the
/// trade's size meaning two different things.
///
/// Returns lots filled by the modify itself: 0 while the order rests, more
/// if the new price crossed the book and it filled on the spot.
pub fn modify_order(zorro_id: i32, new_limit: f64, new_amount: i32) -> Result<i32, String> {
    let plugin = state::get().ok_or("not logged in")?;
    let info = state::get_trade(zorro_id).ok_or("unknown trade id")?;
    if info.closed {
        return Err("the trade is closed".to_string());
    }
    if new_limit <= 0.0 {
        return Err("a modify needs a limit price".to_string());
    }

    let (sz, limit_px, response) = runtime::get().block_on(async {
        // An order placed a moment ago can be accepted before the order
        // lookup has indexed it, and then reads as unknown; give it a second.
        let mut order = None;
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            let status = plugin
                .info_client
                .query_order_by_oid(plugin.address, info.oid)
                .await
                .map_err(|e| format!("order lookup failed: {e}"))?;
            if status.order.is_some() {
                order = status.order;
                break;
            }
        }
        let order = order.ok_or("the exchange no longer knows this order")?;
        if order.status != "open" {
            return Err(format!("the order is {}, nothing to modify", order.status));
        }
        if order.order.sz != order.order.orig_sz {
            return Err(format!(
                "{} of {} has filled; cancel and place a new order instead",
                order.order.orig_sz.parse::<f64>().unwrap_or(0.0)
                    - order.order.sz.parse::<f64>().unwrap_or(0.0),
                order.order.orig_sz
            ));
        }

        let asset = resolve_asset(&plugin.info_client, &info.coin).await?;
        let sz = if new_amount > 0 {
            assets::round_size(new_amount as f64 / AMOUNT_SCALE, asset.sz_decimals)
        } else {
            info.entry_sz
        };
        if sz <= 0.0 {
            return Err(format!("{new_amount} lots rounds to zero size"));
        }
        let limit_px = assets::round_price(new_limit, &asset);
        // Keeps what the order was: a post-only order stays post-only, and
        // a reduce-only close stays reduce-only.
        let tif = order.order.tif.clone().unwrap_or_else(|| "Gtc".to_string());
        // Hyperliquid carries out a modify by cancelling the order and then
        // placing the new one, and it places that one post-only whatever
        // the order was: a GTC order moved through the book is refused with
        // "Post only order would have immediately matched" (live, perp and
        // spot alike), leaving no order at all. So refuse any move through
        // the book here, while the original is still safely resting.
        if let Some(opposite) = best_offer(&plugin.info_client, &info.coin, info.is_buy).await {
            if would_cross(info.is_buy, limit_px, opposite) {
                return Err(format!(
                    "cannot move the order to {limit_px}: the best {} is {opposite}, and Hyperliquid refuses a modify that would trade at once (cancelling the original). The order still rests where it was; to trade now, cancel it and send a market order",
                    if info.is_buy { "ask" } else { "bid" }
                ));
            }
        }
        let request = ClientModifyRequest {
            oid: info.oid,
            order: ClientOrderRequest {
                asset: info.coin.clone(),
                is_buy: info.is_buy,
                reduce_only: order.order.reduce_only,
                limit_px,
                sz,
                cloid: None,
                order_type: ClientOrder::Limit(ClientLimit { tif }),
            },
        };
        let response = plugin
            .exchange_client
            .modify(request, None)
            .await
            .map_err(|e| format!("modify request failed: {e}"))?;
        Ok::<_, String>((sz, limit_px, response))
    })?;

    let status = match response {
        ExchangeResponseStatus::Err(e) => return Err(format!("modify rejected: {e}")),
        ExchangeResponseStatus::Ok(r) => r
            .data
            .and_then(|d| d.statuses.into_iter().next())
            .ok_or("exchange response carried no order status")?,
    };
    // A modify can hand the order a new id; follow it, or BrokerTrade
    // would go on polling an order that no longer exists.
    match status {
        ExchangeDataStatus::Resting(order) => {
            state::set_resting(zorro_id, order.oid, limit_px, sz);
            Ok(0)
        }
        ExchangeDataStatus::Filled(order) => {
            let price = order.avg_px.parse().unwrap_or(limit_px);
            state::set_resting(zorro_id, order.oid, price, sz);
            Ok((sz * AMOUNT_SCALE).round() as i32)
        }
        // The original was cancelled before the replacement was refused.
        // Say so: a script that believes it still has an order working
        // will wait for a fill that cannot come. BrokerTrade reports the
        // order as cancelled, so Zorro's own book catches up as well.
        ExchangeDataStatus::Error(e) => Err(format!(
            "modify refused ({e}); {}",
            if order_still_open(info.oid) {
                "the original order still rests"
            } else {
                "the exchange cancelled the original order, so nothing rests now"
            }
        )),
        other => Err(format!("unexpected modify status: {other:?}")),
    }
}

/// Whether a limit at `limit` would trade against the best opposite price:
/// a buy at or above the ask, a sell at or below the bid.
fn would_cross(is_buy: bool, limit: f64, opposite: f64) -> bool {
    if is_buy {
        limit >= opposite
    } else {
        limit <= opposite
    }
}

fn order_still_open(oid: u64) -> bool {
    let Some(plugin) = state::get() else { return false };
    matches!(
        runtime::get().block_on(plugin.info_client.query_order_by_oid(plugin.address, oid)),
        Ok(status) if status.order.as_ref().is_some_and(|o| o.status == "open")
    )
}

#[cfg(test)]
mod cross_tests {
    use super::would_cross;

    #[test]
    fn a_buy_crosses_at_or_above_the_ask() {
        assert!(would_cross(true, 2245.8, 2245.8));
        assert!(would_cross(true, 2300.0, 2245.8));
        assert!(!would_cross(true, 2245.7, 2245.8));
    }

    #[test]
    fn a_sell_crosses_at_or_below_the_bid() {
        assert!(would_cross(false, 2245.5, 2245.5));
        assert!(would_cross(false, 2200.0, 2245.5));
        assert!(!would_cross(false, 2245.6, 2245.5));
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
///
/// Priced off the far side of the book rather than the mid. The mid is not a
/// price anyone is offering, so on a wide book even a generous slippage
/// margin lands inside the spread and the IOC is rejected outright with
/// "could not immediately match against any resting orders" -- which is how
/// a market buy failed on a pair quoting 3323 / 3390. Crossing the best
/// offer and then adding slippage is what makes the order marketable; the
/// slippage only has to cover the book moving between the read and the fill.
async fn marketable_price(
    info: &InfoClient,
    symbol: &str,
    asset: &AssetInfo,
    is_buy: bool,
    slippage: f64,
) -> Result<f64, String> {
    let reference = match best_offer(info, symbol, is_buy).await {
        Some(px) => px,
        // An empty or unreadable book is not fatal: the mid still produces an
        // order, and one that may rest is better than none at all.
        None => {
            state::log_message(&format!(
                "marketable_price({symbol}): no book, falling back to the mid"
            ));
            mid_price(info, symbol).await?
        }
    };
    let slipped = reference * if is_buy { 1.0 + slippage } else { 1.0 - slippage };
    Ok(assets::round_price(slipped, asset))
}

/// Best price currently offered to the side we are about to take: the lowest
/// ask when buying, the highest bid when selling. `None` if that side of the
/// book is empty or unreadable.
async fn best_offer(info: &InfoClient, symbol: &str, is_buy: bool) -> Option<f64> {
    let book = info.l2_snapshot(symbol.to_string()).await.ok()?;
    // levels[0] is bids, levels[1] is asks, each sorted best-first.
    let side = book.levels.get(if is_buy { 1 } else { 0 })?;
    side.first()?.px.parse().ok()
}

async fn mid_price(info: &InfoClient, symbol: &str) -> Result<f64, String> {
    let mids = info
        .all_mids()
        .await
        .map_err(|e| format!("all_mids failed: {e}"))?;
    mids.get(symbol)
        .ok_or_else(|| format!("no mid price for {symbol}"))?
        .parse()
        .map_err(|_| format!("bad mid price for {symbol}"))
}

/// Cancels a resting order by its Zorro trade id. Backs DO_CANCEL.
pub fn cancel_order(zorro_id: i32) -> Result<(), String> {
    let info = state::get_trade(zorro_id).ok_or_else(|| format!("unknown trade id {zorro_id}"))?;
    if info.twap_id.is_some() {
        return twap::stop(zorro_id, &info);
    }

    cancel_stop(&info.coin, zorro_id);
    cancel_oid(&info.coin, info.oid)
}

/// Cancels one order by its exchange id.
fn cancel_oid(symbol: &str, oid: u64) -> Result<(), String> {
    let plugin = state::get().ok_or("not logged in")?;
    let response = runtime::get()
        .block_on(plugin.exchange_client.cancel(
            ClientCancelRequest {
                asset: symbol.to_string(),
                oid,
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

/// Rests a stop-loss on the exchange for a position just opened, and
/// returns its order id.
///
/// The stop sells a long and buys back a short, so it goes on the opposite
/// side of the entry at `entry +/- stop_dist`. It triggers as a market
/// order: a stop that cannot fill is not a stop. On perps it is
/// reduce-only, so it can only ever shrink the position it protects.
///
/// Why this matters: Zorro's own stops are virtual. It watches the price
/// and sends a close when the level is hit, which protects nothing if Zorro
/// is not running. This one lives on the exchange.
fn place_stop(
    symbol: &str,
    entry_is_buy: bool,
    sz: f64,
    entry_price: f64,
    stop_dist: f64,
) -> Result<u64, String> {
    let plugin = state::get().ok_or("not logged in")?;
    let response = runtime::get().block_on(async {
        let asset = resolve_asset(&plugin.info_client, symbol).await?;
        let raw_trigger = if entry_is_buy {
            entry_price - stop_dist
        } else {
            entry_price + stop_dist
        };
        if raw_trigger <= 0.0 {
            return Err(format!(
                "stop distance {stop_dist} puts the trigger at or below zero"
            ));
        }
        let trigger_px = assets::round_price(raw_trigger, &asset);

        // Triggering is what decides the fill, so the limit only has to be
        // loose enough not to block it. Past the trigger by the same
        // distance leaves room for the gap that set the stop off.
        let limit_px = assets::round_price(
            if entry_is_buy {
                raw_trigger - stop_dist
            } else {
                raw_trigger + stop_dist
            }
            .max(0.0),
            &asset,
        );

        let order = ClientOrderRequest {
            asset: symbol.to_string(),
            is_buy: !entry_is_buy,
            reduce_only: asset.is_perp(),
            limit_px,
            sz: assets::round_size(sz, asset.sz_decimals),
            cloid: None,
            order_type: ClientOrder::Trigger(ClientTrigger {
                is_market: true,
                trigger_px,
                tpsl: "sl".to_string(),
            }),
        };
        plugin
            .exchange_client
            .order(order, None)
            .await
            .map_err(|e| format!("stop request failed: {e}"))
    })?;

    let data = match response {
        ExchangeResponseStatus::Err(e) => return Err(format!("stop rejected: {e}")),
        ExchangeResponseStatus::Ok(r) => r.data,
    };
    match data.and_then(|d| d.statuses.into_iter().next()) {
        // A stop rests until it triggers, which is the expected outcome.
        Some(ExchangeDataStatus::Resting(order)) => Ok(order.oid),
        // Already past the level when it landed, so it fired immediately.
        Some(ExchangeDataStatus::Filled(order)) => Ok(order.oid),
        Some(ExchangeDataStatus::Error(e)) => Err(format!("stop error: {e}")),
        other => Err(format!("unexpected stop status: {other:?}")),
    }
}

/// Cancels a trade's stop, if it still has one resting.
///
/// A stop outliving its position is not harmless: reduce-only or not, it
/// stays live on the exchange and would act on whatever position exists
/// when it triggers.
fn cancel_stop(symbol: &str, zorro_id: i32) {
    let Some(oid) = state::take_stop_oid(zorro_id) else {
        return;
    };
    match cancel_oid(symbol, oid) {
        Ok(()) => state::log_message(&format!("stop for trade {zorro_id} cancelled")),
        // Normal for a reduce-only stop: the exchange retires it with the
        // position it protected, so there is nothing left to cancel.
        Err(e) => state::log_message(&format!("stop for trade {zorro_id} already gone ({e})")),
    }
}

/// What closing part of a trade did, ready for `BrokerSell2`'s out-params.
pub struct CloseOutcome {
    pub symbol: String,
    pub price: f64,
    /// Signed as `place_order` reported it.
    pub fill: i32,
    pub profit: f64,
    /// Negative is money paid: this trade's fees so far plus its funding.
    pub cost: f64,
    /// Fee of this close alone, for logging.
    pub fee: f64,
    /// Size of the trade still open afterwards.
    pub remaining: f64,
}

/// Closes `amount` of a tracked trade and books what it cost against it.
///
/// Lives here rather than in `BrokerSell2` so the fee attribution is
/// reachable without going through the C ABI: the close is placed as its
/// own order, and its fee would otherwise be stranded on a bookkeeping
/// entry Zorro never sees.
pub fn close_trade(trade_id: i32, amount: i32, limit: f64) -> Result<CloseOutcome, String> {
    let mut info = state::get_trade(trade_id).ok_or("unknown trade id")?;
    if amount == 0 {
        return Err("nothing to close".to_string());
    }
    // A TWAP still slicing would keep adding to the position being closed,
    // so stop it first. Zorro asks for the size it last saw, and a slice
    // can fill between its poll and the stop: close everything that filled,
    // or that slice is left open with no trade to close it.
    let mut amount = amount;
    if info.twap_running {
        twap::stop(trade_id, &info)?;
        info = state::get_trade(trade_id).ok_or("unknown trade id")?;
        if info.remaining_sz <= 0.0 {
            return Err("the TWAP filled nothing; it is stopped and there is nothing to close".to_string());
        }
        amount = amount.signum() * (info.remaining_sz * AMOUNT_SCALE).round() as i32;
    }
    let symbol = info.coin.clone();
    let requested = amount.unsigned_abs() as f64 / AMOUNT_SCALE;
    let closes_whole_trade = requested >= info.remaining_sz - 1e-9;

    let plan = spot_close_plan(&symbol, requested, closes_whole_trade, limit)?;
    let (close_id, price, mut fill) = match &plan {
        // A perp: reduce-only takes care of oversized closes.
        None => place_order(&symbol, -amount, limit, true, 0.0)?,
        Some(SpotClose::Sell(sz)) => {
            place_order(&symbol, -((sz * AMOUNT_SCALE).round() as i32), limit, true, 0.0)?
        }
        Some(SpotClose::Unsellable { price, reason }) => {
            if !closes_whole_trade {
                return Err(format!("cannot close {requested} {symbol}: {reason}"));
            }
            return Ok(retire_as_dust(trade_id, &info, &symbol, *price, amount, reason));
        }
    };

    let closed_qty = fill.unsigned_abs() as f64 / AMOUNT_SCALE;
    let direction = if info.is_buy { 1.0 } else { -1.0 };
    let mut profit = direction * (price - info.entry_price) * closed_qty;

    let fee = state::get_trade(close_id).map_or(0.0, |t| t.entry_fee);
    state::add_close_fee(trade_id, fee);
    let cost = -(state::get_trade(trade_id).map_or(fee, |t| t.total_fees())
        + trade_funding(&symbol, closed_qty));

    // Measured against what is left of the trade, not against the amount
    // asked for: a chunk can fill in full while the trade still holds more.
    let mut remaining = state::reduce_trade(trade_id, closed_qty);
    // A whole-trade spot close can sell everything sellable and still leave
    // a sliver the exchange will not take. Retire it with the trade, or
    // Zorro retries a close that cannot fill for as long as it runs.
    if closes_whole_trade && remaining > 0.0 && matches!(plan, Some(SpotClose::Sell(_))) {
        state::log_message(&format!(
            "trade {trade_id}: {remaining} {symbol} left after the close cannot be sold; \
retired with the trade as lost ({})",
            DUST_NOTE
        ));
        profit += retired_profit(info.entry_price, remaining);
        state::reduce_trade(trade_id, remaining);
        remaining = 0.0;
        fill = -amount.abs();
    }
    // Flat now, so its stop has nothing left to protect.
    if remaining <= 0.0 {
        cancel_stop(&symbol, trade_id);
    }

    Ok(CloseOutcome { symbol, price, fill, profit, cost, fee, remaining })
}

/// How a spot close will be carried out.
enum SpotClose {
    /// Sell this size: capped at the balance, whole steps, over the minimum.
    Sell(f64),
    /// Nothing sellable. `price` values what is left for Zorro's P&L.
    Unsellable { price: f64, reason: String },
}

/// For a spot close, what can actually be sold; `None` for a perp, whose
/// close is reduce-only and needs none of this.
///
/// A spot close cannot sell what is not held, nor less than one size step,
/// nor less than $10. Each of those is a hard rejection, and Zorro answers
/// a rejected close by retrying it every few seconds for as long as it
/// runs -- one live run logged dozens of them. Sizing the close to what the
/// exchange will take, here, is what stops that.
fn spot_close_plan(
    symbol: &str,
    requested: f64,
    closes_whole_trade: bool,
    limit: f64,
) -> Result<Option<SpotClose>, String> {
    let plugin = state::get().ok_or("not logged in")?;
    runtime::get().block_on(async {
        let asset = resolve_asset(&plugin.info_client, symbol).await?;
        if asset.is_perp() {
            return Ok(None);
        }
        let held = spot_balance(&plugin.info_client, plugin.address, symbol).await?;
        // The minimum is checked against the price the order will carry,
        // which for a market sell sits under the bid by the slippage margin.
        let price = if limit > 0.0 {
            assets::round_price(limit, &asset)
        } else {
            marketable_price(&plugin.info_client, symbol, &asset, false, 0.05).await?
        };
        let sz = assets::spot_close_size(requested, held, asset.sz_decimals, closes_whole_trade);
        if assets::meets_minimum(sz, price) {
            return Ok(Some(SpotClose::Sell(sz)));
        }
        let reason = if sz <= 0.0 {
            format!("{held} held, under one size step")
        } else {
            format!(
                "{sz} is all that can be sold and is worth {:.2}, under the {} USDC minimum",
                sz * price,
                assets::MIN_ORDER_USD
            )
        };
        Ok(Some(SpotClose::Unsellable { price, reason }))
    })
}

/// Why retired coins are booked as worth nothing, for the log.
const DUST_NOTE: &str = "Hyperliquid sweeps unsellable spot balances from the wallet";

/// What Zorro is told a retired remainder earned: its entry cost, lost.
///
/// A spot balance under one size step can never be sold, and Hyperliquid
/// sweeps it away -- on testnet a leftover of nearly one PURR went within
/// minutes, with no fill, transfer or USDC in return. Booking it at market
/// instead told Zorro each PURR cycle cost far less than the account
/// lost. Only spot buys are retired, so the trade is always long.
fn retired_profit(entry_price: f64, qty: f64) -> f64 {
    -entry_price * qty
}

/// Marks the rest of a trade closed without an order, because no order for
/// it can fill. Zorro stops tracking the coins and books them as lost.
fn retire_as_dust(
    trade_id: i32,
    info: &state::TradeInfo,
    symbol: &str,
    price: f64,
    amount: i32,
    reason: &str,
) -> CloseOutcome {
    let qty = info.remaining_sz;
    state::log_message(&format!(
        "trade {trade_id}: retiring the last {qty} {symbol} unsold ({reason}); booked as lost ({})",
        DUST_NOTE
    ));
    state::reduce_trade(trade_id, qty);
    let total_fees = state::get_trade(trade_id).map_or(info.total_fees(), |t| t.total_fees());
    CloseOutcome {
        symbol: symbol.to_string(),
        price,
        fill: -amount.abs(),
        profit: retired_profit(info.entry_price, qty),
        cost: -total_fees,
        fee: 0.0,
        remaining: 0.0,
    }
}

/// Free balance of `symbol`'s base coin -- the most a spot sell can take.
async fn spot_balance(info: &InfoClient, address: alloy::primitives::Address, symbol: &str) -> Result<f64, String> {
    let spot_meta = info.spot_meta().await.map_err(|e| format!("spot_meta failed: {e}"))?;
    let pair = spot_meta
        .universe
        .iter()
        .find(|a| a.name == symbol)
        .ok_or_else(|| format!("{symbol} is not a spot pair"))?;
    let base = spot_meta
        .tokens
        .iter()
        .find(|t| t.index == pair.tokens[0])
        .ok_or_else(|| format!("no base token for {symbol}"))?;
    let balances = info
        .user_token_balances(address)
        .await
        .map_err(|e| format!("balance read failed: {e}"))?;
    Ok(balances.balances.iter().find(|b| b.coin == base.name).map_or(0.0, |b| {
        let total: f64 = b.total.parse().unwrap_or(0.0);
        let hold: f64 = b.hold.parse().unwrap_or(0.0);
        (total - hold).max(0.0)
    }))
}

/// What an order's fills cost in fees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderFees {
    /// Every fee in USDC, whatever it was paid in. This is what Zorro books.
    pub usd: f64,
    /// The part paid in the coin bought, in that coin. The account received
    /// this much less than the order filled.
    pub in_base: f64,
}

/// Fees the exchange charged for order `oid`, summed over its fills.
/// `None` when the order has no fills in the feed yet -- a fill can be
/// acknowledged before it is indexed, so the caller retries rather than
/// booking zero.
pub fn order_fees(oid: u64) -> Option<OrderFees> {
    let plugin = state::get()?;
    let fills = runtime::get()
        .block_on(plugin.info_client.user_fills(plugin.address))
        .ok()?;
    let mut total = OrderFees { usd: 0.0, in_base: 0.0 };
    let mut seen = false;
    for fill in fills.iter().filter(|f| f.oid == oid) {
        let fee = fill.fee.parse::<f64>().unwrap_or(0.0);
        let px = fill.px.parse::<f64>().unwrap_or(0.0);
        let (usd, in_base) = fee_split(fee, px, &fill.fee_token);
        total.usd += usd;
        total.in_base += in_base;
        seen = true;
    }
    if seen { Some(total) } else { None }
}

/// `order_fees`, waiting briefly for the fills feed to catch up. Only for a
/// spot buy, whose size depends on the fee and so cannot be fixed up later
/// the way a fee alone can.
fn settled_fees(oid: u64) -> Option<OrderFees> {
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if let Some(fees) = order_fees(oid) {
            return Some(fees);
        }
    }
    None
}

/// `(fee in USDC, fee in the traded coin)` for one fill.
///
/// Perps and spot sells pay in USDC. A spot buy pays in the coin bought,
/// and booking that number as USDC undercounted it by the coin's price:
/// a fee of 0.01 PURR went into Zorro as $0.01 rather than $0.05.
fn fee_split(fee: f64, px: f64, fee_token: &str) -> (f64, f64) {
    if fee_token == "USDC" {
        (fee, 0.0)
    } else {
        (fee * px, fee)
    }
}

/// Funding this trade has paid, pro-rated by its share of the position.
///
/// Hyperliquid reports funding per position, not per order, so a position
/// built from several Zorro trades would otherwise charge each of them the
/// whole amount. Spot has no funding and reports zero.
pub fn trade_funding(coin: &str, trade_sz: f64) -> f64 {
    let Some(plugin) = state::get() else { return 0.0 };
    // Spot pays no funding, so skip the round-trip that would find nothing.
    if matches!(state::cached_asset(coin), Some(asset) if !asset.is_perp()) {
        return 0.0;
    }
    let Ok(user_state) = runtime::get().block_on(plugin.info_client.user_state(plugin.address))
    else {
        return 0.0;
    };
    let Some(position) = user_state
        .asset_positions
        .iter()
        .find(|p| p.position.coin == coin)
    else {
        return 0.0;
    };
    let since_open: f64 = position.position.cum_funding.since_open.parse().unwrap_or(0.0);
    let position_sz = position.position.szi.parse::<f64>().unwrap_or(0.0).abs();
    prorate_funding(since_open, position_sz, trade_sz)
}

/// This trade's share of a position's funding. Split out so the rule can be
/// tested without a session.
fn prorate_funding(since_open: f64, position_sz: f64, trade_sz: f64) -> f64 {
    if position_sz <= 0.0 || trade_sz <= 0.0 {
        return 0.0;
    }
    // Capped at the whole amount: a close can report more size than the
    // position still shows once the reduce has settled.
    since_open * (trade_sz / position_sz).min(1.0)
}

#[cfg(test)]
mod fee_tests {
    use super::fee_split;

    #[test]
    fn a_usdc_fee_is_booked_as_is() {
        assert_eq!(fee_split(0.01, 5.0, "USDC"), (0.01, 0.0));
    }

    /// The undercount: 0.01 PURR at $5 is $0.05, not $0.01,
    /// and it also comes off the size the buy delivered.
    #[test]
    fn a_fee_paid_in_the_coin_is_priced_and_taken_off_the_size() {
        let (usd, in_base) = fee_split(0.01, 5.0, "PURR");
        assert!((usd - 0.05).abs() < 1e-9, "got {usd}");
        assert_eq!(in_base, 0.01);
    }
}

#[cfg(test)]
mod funding_tests {
    use super::prorate_funding;

    #[test]
    fn a_whole_position_pays_all_of_its_funding() {
        assert_eq!(prorate_funding(0.42, 1.5, 1.5), 0.42);
    }

    /// Two trades sharing one position must not each be charged the full
    /// amount -- that is double counting, not a rounding difference.
    #[test]
    fn trades_sharing_a_position_split_it() {
        let a = prorate_funding(0.40, 2.0, 1.5);
        let b = prorate_funding(0.40, 2.0, 0.5);
        assert!((a - 0.30).abs() < 1e-12, "got {a}");
        assert!((b - 0.10).abs() < 1e-12, "got {b}");
        assert!((a + b - 0.40).abs() < 1e-12);
    }

    #[test]
    fn a_closed_or_empty_position_pays_nothing() {
        assert_eq!(prorate_funding(0.42, 0.0, 1.0), 0.0);
        assert_eq!(prorate_funding(0.42, 1.0, 0.0), 0.0);
    }

    #[test]
    fn a_share_over_the_position_is_capped() {
        assert_eq!(prorate_funding(0.42, 1.0, 5.0), 0.42);
    }

    /// Funding received, not paid, keeps its sign through the split.
    #[test]
    fn received_funding_stays_negative() {
        assert!((prorate_funding(-0.20, 2.0, 1.0) + 0.10).abs() < 1e-12);
    }
}

/// Status of a tracked order: `(open_price, close_price, cost, profit,
/// fill_state)`; `fill_state` follows BrokerTrade's contract.
pub fn trade_status(zorro_id: i32) -> Option<(f64, f64, f64, f64, i32)> {
    let plugin = state::get()?;
    let info = state::get_trade(zorro_id)?;

    if info.twap_id.is_some() {
        return twap::status(zorro_id, &info);
    }

    // -1 = completely closed. Hyperliquid reports a filled order as "filled"
    // forever, so the flag is the only way Zorro learns the trade is gone.
    if info.closed {
        // Costs survive the close: Zorro books them when it retires the trade.
        return Some((info.entry_price, info.entry_price, -info.total_fees(), 0.0, -1));
    }

    let status = runtime::get()
        .block_on(plugin.info_client.query_order_by_oid(plugin.address, info.oid))
        .ok()?;
    let order_state = status.order.as_ref().map(|o| o.status.clone());
    let open_sz = status.order.as_ref().and_then(|o| o.order.sz.parse::<f64>().ok());
    let all_filled = order_state.as_deref() == Some("filled");

    // Fees before sizes: a spot buy's fee is paid in the coin, and comes off
    // the size reported below. Until the order has filled completely, later
    // pieces bring more fees, so the lookup stays open.
    if !info.entry_fee_final && has_filled(order_state.as_deref(), open_sz, info.entry_sz) {
        if let Some(fees) = order_fees(info.oid) {
            state::book_entry_fees(zorro_id, fees.usd, fees.in_base, all_filled);
        }
    }
    if all_filled {
        state::fill_resting(zorro_id);
    }
    let info = state::get_trade(zorro_id)?;

    let close_price = asset_price(&info.coin).unwrap_or(info.entry_price);
    let direction = if info.is_buy { 1.0 } else { -1.0 };

    // fill_state is in AMOUNT_SCALE sub-units, like BrokerBuy2's fill_amount.
    // filled_sz drives profit: an order still resting has none, so it must
    // not accrue P&L just because the market moved away from its limit.
    let (fill_state, filled_sz) = match order_state.as_deref() {
        // Testnet calls a just-placed order unknown for a moment, and an
        // order the exchange accepted is never forgotten: report the last
        // known state and ask again on the next poll. Reading unknown as
        // closed made Zorro drop a freshly resting limit order.
        None => last_known_fill(&info),
        // Report what is still open, not the original fill: partial
        // closes shrink the trade and Zorro tracks it by this number.
        Some("filled") => last_known_fill(&info),
        Some("canceled" | "rejected" | "marginCanceled") => (NAY - 1, 0.0),
        // Partly filled and still resting: what filled, less any fee taken
        // in the coin so far.
        Some(_) => {
            let remaining = open_sz.unwrap_or(info.entry_sz);
            let filled = (info.entry_sz - remaining - info.entry_fee_in_coin).max(0.0);
            ((filled * AMOUNT_SCALE).round() as i32, filled)
        }
    };
    let profit = direction * (close_price - info.entry_price) * filled_sz;
    // pProfit is gross by contract ("without rollover and commission"), so
    // fees and funding go out through pCost instead. Negative is money paid.
    let fees = state::get_trade(zorro_id).map_or_else(|| info.total_fees(), |t| t.total_fees());
    let cost = -(fees + trade_funding(&info.coin, filled_sz));

    Some((info.entry_price, close_price, cost, profit, fill_state))
}

/// Whether an order in `state`, with `open_sz` of its `entry_sz` still
/// resting, has filled any of it -- and so has fees to look up.
fn has_filled(state: Option<&str>, open_sz: Option<f64>, entry_sz: f64) -> bool {
    match state {
        Some("filled") => true,
        None | Some("canceled" | "rejected" | "marginCanceled") => false,
        Some(_) => open_sz.is_some_and(|open| open < entry_sz),
    }
}

/// `(fill_state, filled_sz)` from what the plugin last knew of a trade:
/// nothing filled while it rests, else the size still open.
fn last_known_fill(info: &state::TradeInfo) -> (i32, f64) {
    if info.pending {
        (0, 0.0)
    } else {
        ((info.remaining_sz * AMOUNT_SCALE).round() as i32, info.remaining_sz)
    }
}

#[cfg(test)]
mod unknown_order_tests {
    use super::*;

    fn trade(pending: bool) -> state::TradeInfo {
        state::TradeInfo {
            coin: "ETH".into(),
            oid: 1,
            is_buy: true,
            entry_price: 1264.0,
            entry_sz: 0.0119,
            remaining_sz: 0.0119,
            closed: false,
            entry_fee: 0.0,
            entry_fee_final: false,
            entry_fee_in_coin: 0.0,
            close_fees: 0.0,
            stop_oid: None,
            pending,
            twap_id: None,
            twap_running: false,
        }
    }

    /// Only an order with something filled has fees to look up.
    #[test]
    fn looks_up_fees_once_anything_filled() {
        assert!(has_filled(Some("filled"), Some(0.0), 1.0));
        assert!(has_filled(Some("open"), Some(0.4), 1.0));
        assert!(!has_filled(Some("open"), Some(1.0), 1.0));
        assert!(!has_filled(Some("canceled"), Some(0.4), 1.0));
        assert!(!has_filled(None, None, 1.0));
    }

    /// A spot buy that rests, then fills in two pieces: the fee is paid in
    /// the coin, so what Zorro is told it holds is the size less that fee,
    /// and the lookup stays open until the order has filled completely.
    #[test]
    fn a_resting_spot_buy_is_held_net_of_its_coin_fee() {
        let mut t = trade(true);
        t.entry_sz = 10.0;
        t.remaining_sz = 10.0;
        t.book_entry_fees(0.02, 0.004, false); // first piece, still resting
        assert_eq!((t.entry_sz, t.remaining_sz, t.entry_fee_final), (10.0, 10.0, false));
        t.book_entry_fees(0.05, 0.01, true); // the rest filled
        t.fill_resting();
        assert!(!t.pending);
        assert!((t.remaining_sz - 9.99).abs() < 1e-12, "{}", t.remaining_sz);
        assert_eq!(t.entry_sz, t.remaining_sz);
        assert_eq!(t.entry_fee, 0.05);
        assert!(t.entry_fee_final);
        // A later poll changes nothing.
        t.fill_resting();
        assert!((t.remaining_sz - 9.99).abs() < 1e-12);
    }

    /// Filled at once but the fee not yet indexed: tracked gross, then
    /// corrected when the fee turns up.
    #[test]
    fn a_late_coin_fee_comes_off_a_filled_trade() {
        let mut t = trade(false);
        t.entry_sz = 10.0;
        t.remaining_sz = 10.0;
        t.book_entry_fees(0.05, 0.01, true);
        assert!((t.remaining_sz - 9.99).abs() < 1e-12);
        assert!((t.entry_sz - 9.99).abs() < 1e-12);
    }

    /// A fee in USDC (perps, spot sells) leaves the size alone.
    #[test]
    fn a_usdc_fee_leaves_the_size_alone() {
        let mut t = trade(true);
        t.book_entry_fees(0.01, 0.0, true);
        t.fill_resting();
        assert_eq!(t.remaining_sz, 0.0119);
    }

    /// A resting order the exchange has not indexed yet stays pending.
    #[test]
    fn an_unknown_resting_order_stays_pending() {
        assert_eq!(last_known_fill(&trade(true)), (0, 0.0));
    }

    /// A filled trade keeps reporting its open size, never -1 (closed).
    #[test]
    fn an_unknown_filled_trade_stays_open() {
        assert_eq!(last_known_fill(&trade(false)), (1190, 0.0119));
    }
}

/// Whether the account runs in a mode where spot USDC backs the perps
/// (`unifiedAccount`, `portfolioMargin`). Asked once per address: the mode
/// rarely changes, and BrokerAccount is polled constantly. A failed query
/// is not remembered, so it is asked again next time.
fn is_unified(plugin: &state::PluginState) -> bool {
    static MODE: std::sync::Mutex<Option<(String, bool)>> = std::sync::Mutex::new(None);
    let address = format!("{:#x}", plugin.address);
    if let Some((known, unified)) = MODE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *known == address {
            return *unified;
        }
    }
    let body = format!(r#"{{"type":"userAbstraction","user":"{address}"}}"#);
    let Ok(reply) = runtime::get().block_on(plugin.info_client.http_client.post("/info", body)) else {
        return false;
    };
    let unified = unified_mode(&reply);
    *MODE.lock().unwrap_or_else(|e| e.into_inner()) = Some((address, unified));
    unified
}

/// Reads a `userAbstraction` reply, a bare JSON string such as
/// `"unifiedAccount"`.
fn unified_mode(reply: &str) -> bool {
    matches!(reply.trim().trim_matches('"'), "unifiedAccount" | "portfolioMargin")
}

/// `(balance, trade_val)` from the raw figures. Zorro's balance is cash and
/// trade value is open P&L, so equity is their sum.
///
/// - Unified: one USDC balance, shown in spot, backs everything. The perp
///   account value is a slice of it (spot "hold") plus open P&L, so adding
///   it to spot counted the margin twice: measured on testnet, the
///   balance read high by the margin held, and higher still with a
///   position open.
/// - Classic: spot and perp are separate wallets. Perp account value
///   includes open P&L, so only its cash part goes into the balance.
fn combine_balances(unified: bool, spot_usdc: f64, perp_value: f64, unrealized: f64) -> (f64, f64) {
    if unified {
        (spot_usdc, unrealized)
    } else {
        (spot_usdc + perp_value - unrealized, unrealized)
    }
}

#[cfg(test)]
mod balance_tests {
    use super::*;

    /// Spot cash with a perp open: the perp account value is not added.
    #[test]
    fn a_unified_account_counts_its_margin_once() {
        let (balance, trade_val) = combine_balances(true, 1000.0, 30.0, 0.5);
        assert_eq!(balance, 1000.0);
        assert_eq!(trade_val, 0.5);
    }

    #[test]
    fn a_classic_account_adds_its_perp_cash() {
        let (balance, trade_val) = combine_balances(false, 100.0, 50.5, 0.5);
        assert!((balance - 150.0).abs() < 1e-9, "open P&L is trade value, not cash: {balance}");
        assert_eq!(trade_val, 0.5);
    }

    #[test]
    fn reads_the_abstraction_reply() {
        assert!(unified_mode("\"unifiedAccount\""));
        assert!(unified_mode("\"portfolioMargin\"\n"));
        assert!(!unified_mode("\"disabled\""));
        assert!(!unified_mode("\"dexAbstraction\""));
        assert!(!unified_mode("null"));
    }
}

/// `(balance, trade_val, margin_val)` for `BrokerAccount`: cash, open P&L,
/// and margin in use, which is what limits further trading. How spot and
/// perp figures combine depends on the account mode; see `combine_balances`.
pub fn account_balance() -> Option<(f64, f64, f64)> {
    let plugin = state::get()?;
    let unified = is_unified(plugin);
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

    let (balance, trade_val) = combine_balances(unified, spot_usdc, account_value, unrealized);
    Some((balance, trade_val, margin_used))
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
///
/// Returns whether anything changed on the exchange: spot accepts 1x as a
/// no-op, which is not worth a log line before every order.
pub fn set_leverage(symbol: &str, leverage: u32) -> Result<bool, String> {
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
        None => Ok(false), // spot: nothing to set
        Some(ExchangeResponseStatus::Err(e)) => Err(format!("leverage rejected: {e}")),
        Some(ExchangeResponseStatus::Ok(_)) => {
            state::cache_leverage(symbol, leverage);
            Ok(true)
        }
    }
}

/// Daily funding per lot as Zorro's `(RollLong, RollShort)` for a perp;
/// `(0, 0)` for spot or when the rate cannot be read.
///
/// Zorro books a positive roll as money received. Hyperliquid's rate is
/// hourly and a positive one means longs pay shorts, so a long's roll is
/// the negated daily rate times the notional of one lot.
pub fn rollover_per_lot(symbol: &str) -> (f64, f64) {
    match perp_funding(symbol) {
        Some((rate, oracle)) => roll_per_lot(rate, oracle),
        None => (0.0, 0.0),
    }
}

/// The sign and scale rule of `rollover_per_lot`, apart from the network.
fn roll_per_lot(hourly_rate: f64, oracle_px: f64) -> (f64, f64) {
    let daily = hourly_rate * 24.0 * oracle_px / AMOUNT_SCALE;
    (-daily, daily)
}

/// `(hourly funding rate, oracle price)` for a perp, from a snapshot of the
/// whole perp universe refreshed at most once a minute. `None` for spot,
/// which has no funding and is absent from the perp universe.
fn perp_funding(symbol: &str) -> Option<(f64, f64)> {
    if let Some(cached) = state::cached_funding(symbol) {
        return cached;
    }
    let plugin = state::get()?;
    let (meta, contexts) = runtime::get()
        .block_on(plugin.info_client.meta_and_asset_contexts())
        .ok()?;
    let rates = meta
        .universe
        .iter()
        .zip(contexts.iter())
        .filter_map(|(asset, ctx)| {
            let rate = ctx.funding.parse::<f64>().ok()?;
            let oracle = ctx.oracle_px.parse::<f64>().ok()?;
            Some((asset.name.clone(), (rate, oracle)))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let found = rates.get(symbol).copied();
    state::cache_funding(rates);
    found
}

#[cfg(test)]
mod roll_tests {
    use super::{roll_per_lot, AMOUNT_SCALE};

    /// ETH at $2,000 with 0.00125% an hour: a coin costs a long $0.60 a
    /// day, and a lot is 1/AMOUNT_SCALE of a coin.
    #[test]
    fn a_positive_rate_charges_longs_and_pays_shorts() {
        let (long, short) = roll_per_lot(0.0000125, 2000.0);
        let per_coin = 0.6;
        assert!((long + per_coin / AMOUNT_SCALE).abs() < 1e-15, "got {long}");
        assert!((short - per_coin / AMOUNT_SCALE).abs() < 1e-15, "got {short}");
    }

    #[test]
    fn a_negative_rate_pays_longs() {
        let (long, short) = roll_per_lot(-0.00001, 3000.0);
        assert!(long > 0.0 && short < 0.0);
    }

    #[test]
    fn no_rate_no_roll() {
        assert_eq!(roll_per_lot(0.0, 2000.0), (-0.0, 0.0));
    }
}

/// What `BrokerAsset` reports: `(ask, ask - bid)`, as its contract asks.
///
/// From the live feed when it has a book for `symbol`. Otherwise the mid
/// over HTTP with no spread, which is what every poll used to return.
pub fn quote(symbol: &str) -> Option<(f64, f64)> {
    crate::feed::maintain();
    for line in crate::feed::take_log() {
        state::log_message(&line);
    }
    if let Some(book) = crate::feed::book(symbol) {
        return Some((book.ask, book.ask - book.bid));
    }
    asset_price(symbol).map(|mid| (mid, 0.0))
}

/// Mid price of `symbol`: from the live feed while it is up, else over
/// HTTP via `all_mids`.
pub fn asset_price(symbol: &str) -> Option<f64> {
    if let Some(mid) = crate::feed::mid(symbol) {
        return Some(mid);
    }
    let plugin = state::get()?;
    let mids = runtime::get()
        .block_on(plugin.info_client.all_mids())
        .ok()?;
    mids.get(symbol)?.parse().ok()
}

/// Historical candles for `symbol`, newest first (BrokerHistory2 contract).
///
/// The window is clamped to the present. Zorro asks for a range that can end
/// after the last closed bar, and may leave `end_ole` at 0 to mean "up to
/// now"; both used to be converted straight through, so the span was measured
/// back from a time that has not happened yet. Only the sliver overlapping
/// real data held candles, which is how a 203-bar lookback returned one bar
/// and left Zorro with an unfillable price series.
pub fn history(
    symbol: &str,
    start_ole: f64,
    end_ole: f64,
    n_tick_minutes: i32,
    n_ticks: i32,
) -> Option<Vec<T6>> {
    let plugin = state::get()?;
    let interval = interval_for_minutes(n_tick_minutes);
    let bar_ms = n_tick_minutes.max(1) as i64 * 60_000;
    let span_ms = n_ticks.max(1) as i64 * bar_ms;

    let (start_ms, end_ms) = history_window(start_ole, end_ole, span_ms, now_unix_millis());

    let candles = runtime::get().block_on(plugin.info_client.candles_snapshot(
        symbol.to_string(),
        interval.to_string(),
        start_ms as u64,
        end_ms as u64,
    ));
    let mut candles = match candles {
        Ok(candles) => candles,
        Err(e) => {
            state::log_message(&format!("history({symbol} {interval}): request failed: {e}"));
            return None;
        }
    };

    let received = candles.len();
    candles.sort_by_key(|c| std::cmp::Reverse(c.time_open)); // oldest-first -> newest-first
    candles.truncate(n_ticks.max(0) as usize);
    // Logged because a short history is silent on Zorro's side until the
    // lookback fails, and the window is the only thing that explains it.
    state::log_message(&format!(
        "history({symbol} {interval}): asked {n_ticks} bars in {start_ole:.5}..{end_ole:.5}, used {start_ms}..{end_ms}, got {received}, returning {}",
        candles.len()
    ));
    Some(candles.into_iter().map(candle_to_t6).collect())
}

/// The millisecond window to request, in `(start, end)`.
///
/// `end` never runs past `now`: a request for candles that do not exist yet
/// comes back empty rather than erroring, so an unclamped end silently
/// starves the series. `start` prefers Zorro's own range start and falls back
/// to counting `span_ms` back from the end when Zorro leaves it unset, which
/// it is free to do.
fn history_window(start_ole: f64, end_ole: f64, span_ms: i64, now_ms: i64) -> (i64, i64) {
    let requested_end = ole_to_unix_millis(end_ole);
    let end_ms = if requested_end <= 0 {
        now_ms
    } else {
        requested_end.min(now_ms)
    };
    let requested_start = ole_to_unix_millis(start_ole);
    let span_start = end_ms - span_ms;
    let start_ms = if requested_start <= 0 || requested_start > end_ms {
        span_start
    } else {
        requested_start.min(span_start)
    };
    (start_ms.max(0), end_ms)
}

fn now_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
mod history_window_tests {
    use super::{history_window, unix_millis_to_ole};

    const NOW: i64 = 1_793_527_380_000;
    const SPAN: i64 = 203 * 60_000;

    /// The live failure: Zorro asked for a range ending ahead of the last
    /// closed bar, so every candle in the window was still in the future and
    /// 203 bars of lookback came back as one.
    #[test]
    fn a_window_ending_in_the_future_is_pulled_back_to_now() {
        let future = unix_millis_to_ole(NOW + SPAN);
        let (start, end) = history_window(0.0, future, SPAN, NOW);
        assert_eq!(end, NOW, "end should not outrun the present");
        assert_eq!(start, NOW - SPAN, "a full span should still be asked for");
    }

    /// Zorro may leave the end unset to mean "up to now". Converting that 0
    /// gives a large negative instant, which the request casts to an enormous
    /// unsigned end time.
    #[test]
    fn an_unset_end_means_now() {
        let (start, end) = history_window(0.0, 0.0, SPAN, NOW);
        assert_eq!(end, NOW);
        assert_eq!(start, NOW - SPAN);
    }

    #[test]
    fn a_past_window_is_left_alone() {
        let end_ms = NOW - 10 * 86_400_000;
        let (start, end) = history_window(0.0, unix_millis_to_ole(end_ms), SPAN, NOW);
        assert_eq!(end, end_ms);
        assert_eq!(start, end_ms - SPAN);
    }

    /// A start Zorro supplies that reaches further back than the span is
    /// honoured, so a chunked fetch is not cut short of what it asked for.
    #[test]
    fn a_wider_start_from_zorro_wins() {
        let end_ms = NOW - 60_000;
        let start_ms = end_ms - 10 * SPAN;
        let (start, end) = history_window(
            unix_millis_to_ole(start_ms),
            unix_millis_to_ole(end_ms),
            SPAN,
            NOW,
        );
        assert_eq!(end, end_ms);
        assert_eq!(start, start_ms);
    }

    /// Nonsense from Zorro must not produce a window the exchange rejects.
    #[test]
    fn a_start_after_the_end_falls_back_to_the_span() {
        let end_ms = NOW - 60_000;
        let (start, end) = history_window(
            unix_millis_to_ole(NOW + 86_400_000),
            unix_millis_to_ole(end_ms),
            SPAN,
            NOW,
        );
        assert_eq!(end, end_ms);
        assert_eq!(start, end_ms - SPAN);
        assert!(start < end);
    }

    #[test]
    fn the_window_never_goes_negative() {
        let (start, _) = history_window(0.0, unix_millis_to_ole(1_000), SPAN, NOW);
        assert_eq!(start, 0);
    }
}

mod deadman;
pub use deadman::{
    forget as forget_deadman, off as deadman_off, renew as renew_deadman, seconds as deadman_seconds,
    set as set_deadman,
};

mod twap;
pub use twap::{minutes as twap_minutes, set_minutes as set_twap_minutes};
#[cfg(test)]
use twap::{cancel_by_id as cancel_twap_by_id, running_twaps};

#[cfg(test)]
mod tests;
