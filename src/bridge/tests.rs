//! Live smoke tests against real Hyperliquid testnet. All `#[ignore]`d by
//! default since they need network access and `HL_TESTNET_PRIVKEY`; run
//! explicitly with `cargo test --lib -- --ignored --nocapture [name]`.
//! Named by what they verify, not by which implementation phase added them.

use super::*;

/// `(pair_name, base_token_name)` for the first entry in `spot_meta()`'s
/// universe, used as "some spot pair to test against" throughout -- on
/// Hyperliquid testnet that's PURR/USDC with base token name "PURR".
fn test_pair(plugin: &state::PluginState) -> (String, String) {
    let spot_meta = runtime::get()
        .block_on(plugin.info_client.spot_meta())
        .expect("spot_meta should succeed");
    let asset = spot_meta.universe.first().unwrap();
    let base_token = spot_meta
        .tokens
        .iter()
        .find(|t| t.index == asset.tokens[0])
        .unwrap();
    (asset.name.clone(), base_token.name.clone())
}

/// Sells any held balance of `base_token_name` down to (near) zero, at a
/// wide slippage buffer to maximize fill odds for what's usually a dust
/// amount. Returns the amount it attempted to sell (0.0 if nothing was held).
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

    let limit_px = runtime::get()
        .block_on(marketable_spot_price(&plugin.info_client, pair_name, false, 0.15))
        .expect("marketable price for flatten sell");
    let order = ClientOrderRequest {
        asset: pair_name.to_string(),
        is_buy: false,
        reduce_only: false,
        limit_px,
        sz: held,
        cloid: None,
        order_type: ClientOrder::Limit(ClientLimit {
            tif: "Ioc".to_string(),
        }),
    };
    let response = runtime::get()
        .block_on(plugin.exchange_client.order(order, None))
        .expect("flatten sell order request");
    println!("flatten sell of {held} {base_token_name}: {response:?}");
    held
}

/// Verifies login, and the read-only paths behind `BrokerAsset` (live price)
/// and `BrokerHistory2` (candles). Never places an order, so it never
/// spends testnet USDC.
#[test]
#[ignore]
fn connects_and_reads_market_data() {
    login().expect("login should succeed");
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

/// Verifies the order lifecycle behind `BrokerBuy2`/`BrokerTrade`/
/// `BrokerAccount`: buys the smallest size that clears Hyperliquid's $10
/// minimum order notional, checks trade status, sells back to close, then
/// flattens any leftover (IOC orders can partially fill) so the account
/// ends up flat regardless of how the sell leg filled. This DOES spend a
/// small amount of testnet USDC -- the order's notional value, round-tripped
/// back out again, so the net cost is only slippage (a few cents at most).
#[test]
#[ignore]
fn places_and_closes_a_spot_order() {
    login().expect("login should succeed");
    let plugin = state::get().unwrap();
    let (pair, base_token) = test_pair(plugin);

    let (balance_before, ..) = account_balance().expect("account_balance before");
    println!("USDC balance before: {balance_before}");

    // Hyperliquid rejects orders below $10 notional; size up to just clear
    // that floor (with a small buffer for price movement) rather than
    // hardcoding a unit count that could fall under it.
    let price = asset_price(&pair).expect("price for sizing");
    let units = ((11.0 / price).ceil() as i32).max(1);
    println!(
        "{pair} price {price}, buying {units} units (~{} USDC notional)",
        units as f64 * price
    );

    let (trade_id, buy_price, buy_fill) =
        place_order(&pair, units, 0.0).expect("buy order should succeed");
    println!("bought {buy_fill} {pair} @ {buy_price}, trade id {trade_id}");
    assert!(buy_fill > 0, "expected the IOC market buy to fill");

    let status = trade_status(trade_id).expect("trade_status after buy");
    println!("trade_status after buy: {status:?}");

    let (close_id, sell_price, sell_fill) =
        place_order(&pair, -units, 0.0).expect("sell order should succeed");
    println!("sold {sell_fill} {pair} @ {sell_price}, trade id {close_id}");
    assert!(sell_fill > 0, "expected the IOC market sell to fill");

    // IOC orders can partially fill against a thin book; don't assume the
    // sell leg fully closed the position -- verify against the real
    // balance and sweep any remainder instead of leaving it open.
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

/// Standalone cleanup: sells any leftover base-token balance for the test
/// pair. Not part of normal test runs -- use after a prior run leaves a
/// position open (e.g. an assertion failed partway through).
#[test]
#[ignore]
fn flattens_leftover_balance() {
    login().expect("login should succeed");
    let plugin = state::get().unwrap();
    let (pair, base_token) = test_pair(plugin);

    let held = flatten_base_balance(plugin, &pair, &base_token);
    if held == 0.0 {
        println!("no {base_token} balance to flatten");
    }
}
