//! Live smoke tests against real Hyperliquid testnet. All `#[ignore]`d by
//! default since they need network access and `HL_TESTNET_PRIVKEY`; run
//! explicitly with `cargo test --lib -- --ignored --nocapture [name]`.
//! Named by what they verify, not by which implementation phase added them.

use super::*;

/// Spot pair the tests trade: HYPE/USDC, which Hyperliquid lists under its
/// index rather than a friendly name. Liquid enough that a $10 order fills
/// without meaningful slippage.
const TEST_SPOT: &str = "@1035";

/// Perp the tests trade: liquid on testnet, with `sz_decimals` fine enough
/// to reach the $10 minimum.
const TEST_PERP: &str = "ETH";

/// Diagnostic: prints every spot pair on whichever network
/// `HL_TESTNET_PRIVKEY` points at. Use it to find the index of a pair that
/// has no canonical name.
#[test]
#[ignore]
fn lists_spot_pairs() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    let plugin = state::get().unwrap();
    let spot_meta = runtime::get()
        .block_on(plugin.info_client.spot_meta())
        .expect("spot_meta should succeed");
    let token_name = |idx: usize| {
        spot_meta
            .tokens
            .iter()
            .find(|t| t.index == idx)
            .map(|t| t.name.as_str())
            .unwrap_or("?")
    };
    for asset in &spot_meta.universe {
        println!(
            "{} (base={}, quote={})",
            asset.name,
            token_name(asset.tokens[0]),
            token_name(asset.tokens[1])
        );
    }
}

/// `(pair_name, base_token_name)` for `TEST_SPOT`, e.g. `("@1035", "HYPE")`.
fn test_pair(plugin: &state::PluginState) -> (String, String) {
    let spot_meta = runtime::get()
        .block_on(plugin.info_client.spot_meta())
        .expect("spot_meta should succeed");
    let asset = spot_meta
        .universe
        .iter()
        .find(|a| a.name == TEST_SPOT)
        .unwrap_or_else(|| panic!("{TEST_SPOT} not in the spot universe"));
    let base_token = spot_meta
        .tokens
        .iter()
        .find(|t| t.index == asset.tokens[0])
        .expect("base token");
    (asset.name.clone(), base_token.name.clone())
}

/// Hyperliquid rejects any order worth less than this.
const MIN_NOTIONAL_USD: f64 = 10.0;

/// Headroom over the minimum. `place_order` rounds the size down to the
/// asset's `sz_decimals`, and the price moves between sizing and filling;
/// either can drop a $10-targeted order under the limit.
const NOTIONAL_BUFFER: f64 = 1.2;

/// Smallest quantity of an asset priced at `price` that still clears the
/// minimum after rounding. Deliberately fractional -- rounding up to whole
/// units would buy ~$160 of a $160 asset.
fn minimum_qty(price: f64) -> f64 {
    if price <= 0.0 {
        return 0.0;
    }
    MIN_NOTIONAL_USD * NOTIONAL_BUFFER / price
}

/// Sells any held balance of `base_token_name` via `place_order` (reuses
/// its sz_decimals rounding). Returns the amount actually sold, or 0.0 if
/// nothing was held or the sale failed.
fn flatten_base_balance(plugin: &state::PluginState, pair_name: &str, base_token_name: &str) -> f64 {
    let balances = runtime::get()
        .block_on(plugin.info_client.user_token_balances(plugin.address))
        .expect("user_token_balances");
    let held = balances
        .balances
        .iter()
        .find(|b| b.coin == base_token_name)
        .and_then(|b| b.total.parse::<f64>().ok())
        .unwrap_or(0.0);
    if held <= 0.0 {
        return 0.0;
    }

    let amount = (held * AMOUNT_SCALE).round() as i32;
    match place_order(pair_name, -amount, 0.0, false) {
        Ok((_, px, fill)) => {
            println!("flattened {held} {base_token_name} @ {px} (filled {fill})");
            held
        }
        Err(e) => {
            // Below the $10 minimum this is expected: the dust is unsellable.
            println!("could not flatten {held} {base_token_name}: {e}");
            0.0
        }
    }
}

/// A [Stop]/[Trade] cycle keeps the DLL loaded, so logout drops the session
/// and the next login reconnects. Without that, a changed key or a
/// Demo->Real switch would keep talking to the old session. No orders.
#[test]
#[ignore]
fn logout_then_login_reconnects() {
    let first = login(None, BaseUrl::Testnet).expect("first login should succeed");
    assert!(state::get().is_some(), "state should be set after login");

    state::clear();
    assert!(state::get().is_none(), "logout should drop the session");

    let second = login(None, BaseUrl::Testnet).expect("re-login should succeed");
    assert_eq!(first, second, "same key should give the same address");
    assert!(state::get().is_some(), "state should be set again");
    println!("logout/re-login OK, address={second}");
}

/// Verifies login and the read-only BrokerAsset/BrokerHistory2 paths. No orders.
#[test]
#[ignore]
fn connects_and_reads_market_data() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    let plugin = state::get().expect("state should be initialized after login");
    let (pair, _) = test_pair(plugin);
    println!("testing against {pair}");

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

/// Buys just above Hyperliquid's $10 minimum, checks trade status, sells
/// back, flattens any leftover. Spends a little testnet USDC (slippage only).
#[test]
#[ignore]
fn places_and_closes_a_spot_order() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    let plugin = state::get().unwrap();
    let (pair, base_token) = test_pair(plugin);

    let (balance_before, ..) = account_balance().expect("account_balance before");
    println!("USDC balance before: {balance_before}");

    let price = asset_price(&pair).expect("price for sizing");
    let real_qty = minimum_qty(price);
    let units = (real_qty * AMOUNT_SCALE).round() as i32;
    println!("{pair} price {price}, buying {real_qty} (~{:.2} USDC)", real_qty * price);

    let (trade_id, buy_price, buy_fill) =
        place_order(&pair, units, 0.0, false).expect("buy order should succeed");
    println!("bought {buy_fill} {pair} @ {buy_price}, trade id {trade_id}");
    assert!(buy_fill > 0, "expected the IOC market buy to fill");

    let status = trade_status(trade_id).expect("trade_status after buy");
    println!("trade_status after buy: {status:?}");

    let (close_id, sell_price, sell_fill) =
        place_order(&pair, -units, 0.0, true).expect("sell order should succeed");
    println!("sold {sell_fill} {pair} @ {sell_price}, trade id {close_id}");
    assert!(sell_fill > 0, "expected the IOC market sell to fill");

    // IOC can partially fill; sweep any remainder instead of leaving it open
    let leftover = flatten_base_balance(plugin, &pair, &base_token);
    if leftover > 0.0 {
        println!("swept {leftover} leftover {base_token} after partial sell fill");
    }

    let (balance_after, ..) = account_balance().expect("account_balance after");
    println!(
        "USDC balance after: {balance_after} (delta: {})",
        balance_after - balance_before
    );
}

/// Covers the GTC + DO_CANCEL path, which the market-order tests never
/// reach: rests a buy far below market so it cannot fill, confirms the
/// exchange accepted it as resting, then cancels it. Buys nothing -- it
/// only locks a little USDC for as long as the order is open.
#[test]
#[ignore]
fn rests_and_cancels_a_limit_order() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    let plugin = state::get().unwrap();
    let (pair, _) = test_pair(plugin);

    state::set_order_type(ORDER_TYPE_GTC);

    let price = asset_price(&pair).expect("price for sizing");
    let limit_px = price * 0.6; // deep below market: must not fill
    // the $10 minimum applies at the limit price, not the market price
    let real_qty = minimum_qty(limit_px);
    let units = (real_qty * AMOUNT_SCALE).round() as i32;
    println!("{pair} mid {price}, resting a buy for {real_qty} @ {limit_px}");

    let (trade_id, rest_price, fill) =
        place_order(&pair, units, limit_px, false).expect("limit order should be accepted");
    assert_eq!(fill, 0, "a buy 40% below market must not fill");
    println!("resting as trade id {trade_id} @ {rest_price}");

    let status = trade_status(trade_id).expect("trade_status while resting");
    println!("trade_status while resting: {status:?}");
    let (.., profit, fill_state) = status;
    assert_eq!(fill_state, 0, "a resting order has filled nothing");
    assert_eq!(profit, 0.0, "a resting order must not accrue profit");

    cancel_order(trade_id).expect("cancel should succeed");
    println!("cancelled {trade_id}");

    // a second cancel must fail cleanly rather than panic or hang
    let second = cancel_order(trade_id);
    println!("second cancel (expected to fail): {second:?}");
    assert!(second.is_err(), "cancelling an already-cancelled order should error");

    state::set_order_type(1); // back to IOC for any later test in this process
}

/// Perps and spot round prices differently (6 vs 8 max decimals before
/// `sz_decimals`). Resolving the universe wrongly means every order is
/// rejected as an invalid price, so check the classification itself.
#[test]
#[ignore]
fn resolves_perp_and_spot_universes() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    let plugin = state::get().unwrap();
    let (spot_pair, _) = test_pair(plugin);

    let perp = runtime::get()
        .block_on(resolve_asset(&plugin.info_client, TEST_PERP))
        .expect("perp should resolve");
    let spot = runtime::get()
        .block_on(resolve_asset(&plugin.info_client, &spot_pair))
        .expect("spot should resolve");

    println!("{TEST_PERP}: {perp:?} -> {} price decimals", perp.max_price_decimals());
    println!("{spot_pair}: {spot:?} -> {} price decimals", spot.max_price_decimals());

    assert!(perp.is_perp(), "{TEST_PERP} must resolve as a perp");
    assert!(!spot.is_perp(), "{spot_pair} must resolve as spot");
    assert!(perp.max_leverage > 1, "a perp should allow leverage");
    assert_eq!(
        perp.max_price_decimals() + 2,
        spot_decimals_for(&spot, perp.sz_decimals),
        "perp prices must allow exactly two fewer decimals than spot at equal sz_decimals"
    );
}

/// Spot allowance at a given `sz_decimals`, for comparison against a perp.
fn spot_decimals_for(_spot: &crate::assets::AssetInfo, sz_decimals: u32) -> u32 {
    8u32.saturating_sub(sz_decimals)
}

/// A short is the thing spot could never express. Opens one, checks the
/// position reports **negative**, then closes it with `closing = true` so
/// the order is reduce-only and cannot overshoot into a long.
#[test]
#[ignore]
fn opens_and_closes_a_perp_short() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    set_leverage(TEST_PERP, 1).expect("1x leverage keeps this unleveraged");
    state::set_order_type(1); // IOC

    let flat = net_position(TEST_PERP).expect("net_position");
    assert!(flat.abs() < 1e-9, "start flat, found {flat}");

    let price = asset_price(TEST_PERP).expect("price for sizing");
    let qty = minimum_qty(price);
    let units = (qty * AMOUNT_SCALE).round() as i32;
    println!("{TEST_PERP} @ {price}, shorting {qty} (~{:.2} USDC)", qty * price);

    let (id, fill_px, fill) = place_order(TEST_PERP, -units, 0.0, false).expect("short should open");
    assert!(fill > 0, "expected the short to fill");
    println!("short opened: {fill} @ {fill_px}, trade id {id}");

    let shorted = net_position(TEST_PERP).expect("net_position after short");
    println!("net position while short: {shorted}");
    assert!(shorted < 0.0, "a short must report a NEGATIVE position, got {shorted}");

    // closing=true -> reduce_only, so an oversized close flattens, never flips
    let (close_id, close_px, close_fill) =
        place_order(TEST_PERP, units, 0.0, true).expect("short should close");
    println!("short closed: {close_fill} @ {close_px}, trade id {close_id}");

    let after = net_position(TEST_PERP).expect("net_position after close");
    println!("net position after close: {after}");
    assert!(after.abs() < qty * 0.5, "position should be ~flat, got {after}");
}

/// reduce_only must stop a close from overshooting into an opposing
/// position -- the failure that would turn "close my long" into "open a
/// short" and quietly double the exposure.
#[test]
#[ignore]
fn oversized_close_flattens_instead_of_flipping() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    set_leverage(TEST_PERP, 1).expect("set leverage");
    state::set_order_type(1);

    let price = asset_price(TEST_PERP).expect("price");
    let qty = minimum_qty(price);
    let units = (qty * AMOUNT_SCALE).round() as i32;

    place_order(TEST_PERP, units, 0.0, false).expect("long should open");
    let long = net_position(TEST_PERP).expect("net_position");
    assert!(long > 0.0, "expected a long, got {long}");
    println!("long open: {long}");

    // close with triple the size; reduce_only must cap it at flat
    place_order(TEST_PERP, -units * 3, 0.0, true).expect("oversized close");
    let after = net_position(TEST_PERP).expect("net_position after");
    println!("net position after 3x oversized close: {after}");
    assert!(
        after >= -qty * 0.5,
        "reduce_only failed: close flipped into a short of {after}"
    );
}

/// Standalone cleanup: sells any leftover balance for the test pair.
#[test]
#[ignore]
fn flattens_leftover_balance() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    let plugin = state::get().unwrap();
    let (pair, base_token) = test_pair(plugin);

    let held = flatten_base_balance(plugin, &pair, &base_token);
    if held == 0.0 {
        println!("no {base_token} balance to flatten");
    }
}

/// Leverage must be visible with no position open: the clearinghouse state
/// lists only open positions, so reading it there left `BrokerAsset`
/// reporting 1x margin and Zorro under-sizing the first entry after a
/// leverage change. Re-logs in between the two reads so the session cache
/// is empty and the figure has to come back from the exchange.
#[test]
#[ignore]
fn margin_reflects_leverage_while_flat() {
    login(None, BaseUrl::Testnet).expect("login should succeed");
    let price = asset_price(TEST_PERP).expect("perp price");
    let full_notional = price / AMOUNT_SCALE;

    set_leverage(TEST_PERP, 1).expect("1x should be accepted");
    login(None, BaseUrl::Testnet).expect("re-login should succeed");
    let at_one = margin_cost_per_lot(TEST_PERP, price);

    set_leverage(TEST_PERP, 5).expect("5x should be accepted");
    login(None, BaseUrl::Testnet).expect("re-login should succeed");
    let at_five = margin_cost_per_lot(TEST_PERP, price);

    set_leverage(TEST_PERP, 1).expect("leverage should be restored to 1x");

    assert!(
        (at_one - full_notional).abs() < full_notional * 1e-9,
        "1x margin {at_one} should be the full notional {full_notional}"
    );
    assert!(
        (at_five * 5.0 - at_one).abs() < at_one * 1e-9,
        "5x margin {at_five} should be a fifth of {at_one}"
    );
}
