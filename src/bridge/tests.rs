//! Live smoke tests against real Hyperliquid testnet. All `#[ignore]`d by
//! default since they need network access and `HL_TESTNET_PRIVKEY`; run
//! explicitly with `cargo test --lib -- --ignored --nocapture [name]`.
//! Named by what they verify, not by which implementation phase added them.

use super::*;

/// Diagnostic: prints every spot pair on whichever network
/// `HL_TESTNET_PRIVKEY` points at.
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

/// `(pair_name, base_token_name)` for the first `spot_meta()` entry --
/// Hyperliquid's own canonical spot listing on testnet.
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

/// Sells any held balance of `base_token_name` via `place_order` (reuses
/// its sz_decimals rounding). Returns the amount attempted (0.0 if none held).
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
    let result = place_order(pair_name, -amount, 0.0);
    println!("flatten sell of {held} {base_token_name}: {result:?}");
    held
}

/// A [Stop]/[Trade] cycle keeps the DLL loaded, so logout must actually
/// drop the session and the next login must reconnect -- this used to fail
/// with "already logged in" (OnceLock), then silently reuse the stale
/// session (short-circuit), which would have ignored a changed key or a
/// Demo->Real switch. No orders.
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

    // clear Hyperliquid's $10 minimum with a small buffer; units = AMOUNT_SCALE sub-units
    let price = asset_price(&pair).expect("price for sizing");
    let real_qty = (11.0 / price).ceil().max(1.0);
    let units = (real_qty * AMOUNT_SCALE).round() as i32;
    println!("{pair} price {price}, buying {real_qty} units (~{} USDC notional)", real_qty * price);

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
