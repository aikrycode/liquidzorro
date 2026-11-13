//! TWAP entries: one order the exchange slices into small market orders,
//! one about every 30 seconds, over a set number of minutes.
//!
//! Why: a large market order walks the book and pays for every level it
//! crosses. Sliced over time, each piece takes only the best prices, and
//! the slicing runs on the exchange, so it carries on while Zorro sleeps.
//!
//! Switched on by SET_TWAP with a number of minutes, off with 0, and the
//! entry must be sent as GTC (`setf(TradeMode, TR_GTC)`). To Zorro a TWAP
//! is an ordinary trade that fills gradually: it starts pending, `BrokerTrade` reports its size growing slice by slice, and
//! `exitLong`/`exitShort` stop it and close what filled. `DO_CANCEL` stops
//! it and keeps what filled.
//!
//! Limits: perps only (spot buys pay their fee in the coin, which this
//! does not track); no attached stop; slices are not randomized; and the
//! exchange refuses a TWAP worth under $100.

use std::sync::atomic::{AtomicU32, Ordering};

use serde::Serialize;
use serde_json::Value;

use super::{asset_price, resolve_asset, AMOUNT_SCALE};
use crate::assets;
use crate::ffi::NAY;
use crate::{l1, runtime, state};

/// The exchange's limits on a TWAP's length.
pub const MIN_MINUTES: u32 = 5;
pub const MAX_MINUTES: u32 = 1440;

/// The exchange's smallest TWAP, in USDC ("TWAP order value too small.
/// Min is $100.").
pub const MIN_TWAP_USD: f64 = 100.0;

/// Minutes the next entries run as a TWAP; 0 = off, entries fill at once.
/// A switch of its own rather than an order type, because Zorro resends
/// SET_ORDERTYPE itself at every entry, and its +8 is a stop flag.
static MINUTES: AtomicU32 = AtomicU32::new(0);

/// Sets how long the next entries run as a TWAP, or 0 to stop entering
/// that way. Returns the minutes kept, or `None` if outside the exchange's
/// range (the setting is then unchanged).
pub fn set_minutes(minutes: i32) -> Option<u32> {
    let minutes = u32::try_from(minutes)
        .ok()
        .filter(|m| *m == 0 || (MIN_MINUTES..=MAX_MINUTES).contains(m))?;
    MINUTES.store(minutes, Ordering::Relaxed);
    Some(minutes)
}

pub fn minutes() -> u32 {
    MINUTES.load(Ordering::Relaxed)
}

/// Exchange actions, in the field order the signature hash expects.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Action {
    TwapOrder { twap: TwapWire },
    TwapCancel { a: u32, t: u64 },
}

#[derive(Serialize)]
struct TwapWire {
    /// Asset index.
    a: u32,
    /// Buy.
    b: bool,
    /// Size, wire format.
    s: String,
    /// Reduce-only.
    r: bool,
    /// Minutes.
    m: u32,
    /// Randomize slice timing.
    t: bool,
}

/// Signs and posts one action; the raw reply.
fn send(action: &Action) -> Result<String, String> {
    let plugin = state::get().ok_or("not logged in")?;
    let ex = &plugin.exchange_client;
    let nonce = l1::next_nonce();
    let hash = l1::action_hash(action, nonce, ex.vault_address)?;
    let signature = l1::sign(&ex.wallet, hash, ex.http_client.is_mainnet())?;
    let body = l1::payload(action, &signature, nonce, ex.vault_address)?;
    runtime::get()
        .block_on(ex.http_client.post("/exchange", body))
        .map_err(|e| format!("request failed: {e}"))
}

fn asset_index(symbol: &str) -> Result<u32, String> {
    let plugin = state::get().ok_or("not logged in")?;
    plugin.exchange_client.coin_to_asset.get(symbol).copied().ok_or_else(|| format!("no asset index for {symbol}"))
}

/// Whether Zorro will follow a TWAP entry sent with this SET_ORDERTYPE.
///
/// It starts with nothing filled. Zorro keeps polling such a trade only if
/// it went out as GTC; otherwise it books 0 lots for good and drops the
/// trade without a close (live: the TWAP then bought its whole size into
/// a position Zorro no longer held). So only the GTC bit lets it through.
pub fn entry_is_tracked(order_type: i32) -> bool {
    order_type & 2 != 0
}

/// Starts a TWAP buying (or selling) `raw_sz` of `symbol` over `minutes()`
/// and registers it as a pending Zorro trade.
pub fn place(symbol: &str, is_buy: bool, raw_sz: f64) -> Result<(i32, f64, i32), String> {
    let plugin = state::get().ok_or("not logged in")?;
    let asset = runtime::get().block_on(resolve_asset(&plugin.info_client, symbol))?;
    if !asset.is_perp() {
        return Err(format!("TWAP entries are for perps only; {symbol} is spot"));
    }
    let sz = assets::round_size(raw_sz, asset.sz_decimals);
    let price = asset_price(symbol).ok_or_else(|| format!("no price for {symbol}"))?;
    // Checked here so the log says what it takes, before anything is sent.
    if sz * price < MIN_TWAP_USD {
        return Err(format!(
            "a TWAP must be worth at least ${MIN_TWAP_USD}; {sz} {symbol} is about ${:.2}",
            sz * price
        ));
    }
    let minutes = minutes();
    let action = Action::TwapOrder {
        twap: TwapWire {
            a: asset_index(symbol)?,
            b: is_buy,
            s: l1::size_to_wire(sz, asset.sz_decimals),
            r: false,
            m: minutes,
            t: false,
        },
    };
    let twap_id = placed_id(&send(&action)?)?;
    let zorro_id = state::register_trade(symbol, 0, is_buy, price, sz);
    state::set_twap(zorro_id, twap_id);
    state::log_message(&format!(
        "TWAP {twap_id}: {} {sz} {symbol} over {minutes} min as trade {zorro_id}",
        if is_buy { "buying" } else { "selling" }
    ));
    // Nothing filled yet: Zorro reads 0 as a pending trade and polls it.
    Ok((zorro_id, price, 0))
}

/// Stops a trade's TWAP if it still runs, then records what it filled.
/// Stopping one that already ended is not an error: either way nothing
/// more will fill, which is what the caller wants.
pub fn stop(zorro_id: i32, info: &state::TradeInfo) -> Result<(), String> {
    let twap_id = info.twap_id.ok_or("not a TWAP trade")?;
    if info.twap_running {
        let action = Action::TwapCancel { a: asset_index(&info.coin)?, t: twap_id };
        cancelled(&send(&action)?)?;
    }
    // The history can trail the cancel by a moment; wait until it shows
    // the TWAP ended, so no slice that filled meanwhile is missed.
    for attempt in 0..8 {
        if let Some(p) = progress(twap_id) {
            if !p.running {
                state::set_twap_progress(zorro_id, p.filled, p.avg_price, p.fees, false);
                state::log_message(&format!(
                    "TWAP {twap_id} stopped: {} {} filled at {:.4}",
                    p.filled, info.coin, p.avg_price
                ));
                return Ok(());
            }
        }
        if attempt < 7 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    Err(format!("TWAP {twap_id} was told to stop but still shows as running"))
}

/// BrokerTrade for a TWAP trade: `(open, close, cost, profit, fill_state)`.
pub fn status(zorro_id: i32, info: &state::TradeInfo) -> Option<(f64, f64, f64, f64, i32)> {
    let twap_id = info.twap_id?;
    let mut info = info.clone();
    if info.twap_running {
        // An unreadable history keeps the last known state rather than
        // guessing; the next poll asks again.
        if let Some(p) = progress(twap_id) {
            state::set_twap_progress(zorro_id, p.filled, p.avg_price, p.fees, p.running);
            info = state::get_trade(zorro_id)?;
        }
    }
    let close_price = asset_price(&info.coin).unwrap_or(info.entry_price);
    if info.closed {
        // Ended with nothing filled: gone, like a cancelled order.
        // A stopped TWAP's entry size is what it filled.
        let state = if info.entry_sz <= 0.0 { NAY - 1 } else { -1 };
        return Some((info.entry_price, info.entry_price, -info.total_fees(), 0.0, state));
    }
    let direction = if info.is_buy { 1.0 } else { -1.0 };
    let profit = direction * (close_price - info.entry_price) * info.remaining_sz;
    let cost = -(info.total_fees() + super::trade_funding(&info.coin, info.remaining_sz));
    let fill_state = (info.remaining_sz * AMOUNT_SCALE).round() as i32;
    Some((info.entry_price, close_price, cost, profit, fill_state))
}

/// How far one TWAP has got.
#[derive(Debug, PartialEq)]
struct Progress {
    filled: f64,
    avg_price: f64,
    fees: f64,
    running: bool,
}

fn progress(twap_id: u64) -> Option<Progress> {
    let plugin = state::get()?;
    let ask = |kind: &str| {
        let body = format!(r#"{{"type":"{kind}","user":"{:#x}"}}"#, plugin.address);
        runtime::get().block_on(plugin.info_client.http_client.post("/info", body)).ok()
    };
    progress_from(&ask("twapHistory")?, &ask("userTwapSliceFills")?, twap_id)
}

/// Reads a TWAP's progress from the `twapHistory` and `userTwapSliceFills`
/// replies.
///
/// The history only says whether it still runs: it holds one entry per
/// change of status, and its executed size is written when the TWAP ends.
/// The "activated" entry reads 0 filled the whole time it runs (live:
/// slices had filled, history still 0), so size, price and fees all come
/// from the slice fills.
fn progress_from(history: &str, fills: &str, twap_id: u64) -> Option<Progress> {
    let history: Value = serde_json::from_str(history).ok()?;
    let latest = history
        .as_array()?
        .iter()
        .filter(|e| e["twapId"].as_u64() == Some(twap_id))
        .max_by_key(|e| recency(e))?;
    let running = is_activated(latest);

    let num = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let fills: Value = serde_json::from_str(fills).ok()?;
    let (mut filled, mut notional, mut fees) = (0.0, 0.0, 0.0);
    for fill in fills.as_array()?.iter().filter(|f| f["twapId"].as_u64() == Some(twap_id)) {
        let sz = num(&fill["fill"]["sz"]);
        filled += sz;
        notional += sz * num(&fill["fill"]["px"]);
        fees += num(&fill["fill"]["fee"]);
    }
    let avg_price = if filled > 0.0 { notional / filled } else { 0.0 };
    Some(Progress { filled, avg_price, fees, running })
}

fn is_activated(entry: &Value) -> bool {
    entry["status"]["status"].as_str() == Some("activated")
}

/// Orders a TWAP's history entries, newest last. Times are whole seconds,
/// so one cancelled at once has its start and end in the same second; an
/// ended TWAP never restarts, so on a tie the end is the newer entry.
/// Without this a same-second cancel read as still running.
fn recency(entry: &Value) -> (u64, bool) {
    (entry["time"].as_u64().unwrap_or(0), !is_activated(entry))
}

/// TWAPs on this account still slicing, as `(twap_id, coin)`: for cleanup.
pub fn running_twaps() -> Vec<(u64, String)> {
    let Some(plugin) = state::get() else { return Vec::new() };
    let body = format!(r#"{{"type":"twapHistory","user":"{:#x}"}}"#, plugin.address);
    let Ok(reply) = runtime::get().block_on(plugin.info_client.http_client.post("/info", body)) else {
        return Vec::new();
    };
    running_from(&reply)
}

fn running_from(history: &str) -> Vec<(u64, String)> {
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(history) else { return Vec::new() };
    let mut latest: std::collections::HashMap<u64, &Value> = std::collections::HashMap::new();
    for e in &entries {
        let Some(id) = e["twapId"].as_u64() else { continue };
        let newer = latest.get(&id).map_or(true, |seen| recency(e) > recency(seen));
        if newer {
            latest.insert(id, e);
        }
    }
    let mut running: Vec<(u64, String)> = latest
        .into_iter()
        .filter(|(_, e)| is_activated(e))
        .map(|(id, e)| (id, e["state"]["coin"].as_str().unwrap_or("").to_string()))
        .collect();
    running.sort();
    running
}

/// Stops one TWAP by id, with no Zorro trade behind it: for cleanup.
pub fn cancel_by_id(coin: &str, twap_id: u64) -> Result<(), String> {
    cancelled(&send(&Action::TwapCancel { a: asset_index(coin)?, t: twap_id })?)
}

/// The TWAP id from a `twapOrder` reply, or the exchange's refusal.
fn placed_id(reply: &str) -> Result<u64, String> {
    let v: Value = serde_json::from_str(reply).map_err(|_| format!("unreadable reply: {reply}"))?;
    if v["status"] != "ok" {
        return Err(format!("TWAP refused: {}", v["response"]));
    }
    let status = &v["response"]["data"]["status"];
    if let Some(id) = status["running"]["twapId"].as_u64() {
        return Ok(id);
    }
    match status["error"].as_str() {
        Some(e) => Err(format!("TWAP refused: {e}")),
        None => Err(format!("unexpected TWAP reply: {reply}")),
    }
}

/// Reads a `twapCancel` reply. A TWAP that already ended counts as
/// stopped.
fn cancelled(reply: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(reply).map_err(|_| format!("unreadable reply: {reply}"))?;
    if v["status"] != "ok" {
        return Err(format!("TWAP cancel refused: {}", v["response"]));
    }
    let status = &v["response"]["data"]["status"];
    if status == "success" {
        return Ok(());
    }
    match status["error"].as_str() {
        Some(e) if e.contains("never placed, already canceled, or filled") => Ok(()),
        Some(e) => Err(format!("TWAP cancel refused: {e}")),
        None => Err(format!("unexpected TWAP cancel reply: {reply}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reply shapes as testnet sends them; ids, times and sizes are made up.
    const PLACED: &str = r#"{"status":"ok","response":{"type":"twapOrder","data":{"status":{"running":{"twapId":101}}}}}"#;
    const TOO_SMALL: &str = r#"{"status":"ok","response":{"type":"twapOrder","data":{"status":{"error":"TWAP order value too small. Min is $100."}}}}"#;
    const CANCELLED: &str = r#"{"status":"ok","response":{"type":"twapCancel","data":{"status":"success"}}}"#;
    const GONE: &str = r#"{"status":"ok","response":{"type":"twapCancel","data":{"status":{"error":"TWAP was never placed, already canceled, or filled."}}}}"#;
    const HISTORY: &str = r#"[{"time":1700000120,"state":{"coin":"ETH","side":"B","sz":"0.05","executedSz":"0.015","executedNtl":"31.5","minutes":5},"status":{"status":"terminated"},"twapId":101},{"time":1700000050,"state":{"coin":"ETH","side":"B","sz":"0.05","executedSz":"0.0","executedNtl":"0.0","minutes":5},"status":{"status":"activated"},"twapId":101},{"time":1700000200,"state":{"executedSz":"9","executedNtl":"9"},"status":{"status":"activated"},"twapId":1}]"#;
    const FILLS: &str = r#"[{"fill":{"coin":"ETH","px":"2100","sz":"0.003","fee":"0.003"},"twapId":101},{"fill":{"coin":"ETH","px":"2110","sz":"0.002","fee":"0.002"},"twapId":101},{"fill":{"coin":"ETH","px":"9","sz":"1","fee":"5"},"twapId":1}]"#;

    #[test]
    fn reads_the_twap_id() {
        assert_eq!(placed_id(PLACED), Ok(101));
    }

    #[test]
    fn a_refusal_names_the_reason() {
        assert_eq!(placed_id(TOO_SMALL), Err("TWAP refused: TWAP order value too small. Min is $100.".into()));
    }

    #[test]
    fn an_ended_twap_counts_as_stopped() {
        assert_eq!(cancelled(CANCELLED), Ok(()));
        assert_eq!(cancelled(GONE), Ok(()));
    }

    /// Running or not comes from the newest history entry; size, price and
    /// fees from this TWAP's slice fills only.
    #[test]
    fn progress_comes_from_the_slice_fills() {
        let p = progress_from(HISTORY, FILLS, 101).unwrap();
        assert!((p.filled - 0.005).abs() < 1e-12, "{}", p.filled);
        assert!((p.avg_price - (0.003 * 2100.0 + 0.002 * 2110.0) / 0.005).abs() < 1e-9);
        assert!((p.fees - 0.005).abs() < 1e-9);
        assert!(!p.running);
        let other = progress_from(HISTORY, FILLS, 1).unwrap();
        assert!(other.running && other.filled == 1.0 && other.avg_price == 9.0);
        assert_eq!(progress_from(HISTORY, FILLS, 999), None);
    }

    /// The bug the live test caught: while running, the history entry reads
    /// 0 filled though slices have filled.
    #[test]
    fn a_running_twap_counts_its_slices() {
        let history = r#"[{"time":5,"state":{"coin":"ETH","executedSz":"0.0","executedNtl":"0.0"},"status":{"status":"activated"},"twapId":7}]"#;
        let fills = r#"[{"fill":{"px":"2100","sz":"0.0056","fee":"0.005"},"twapId":7},{"fill":{"px":"2110","sz":"0.0056","fee":"0.005"},"twapId":7}]"#;
        let p = progress_from(history, fills, 7).unwrap();
        assert!(p.running);
        assert!((p.filled - 0.0112).abs() < 1e-12);
        assert!((p.avg_price - 2105.0).abs() < 1e-9);
    }

    /// Live: a TWAP started and was cancelled in the same second, its
    /// end listed first.
    #[test]
    fn a_same_second_cancel_reads_as_ended() {
        let history = r#"[{"time":1700000300,"state":{"coin":"ETH"},"status":{"status":"terminated"},"twapId":102},{"time":1700000300,"state":{"coin":"ETH"},"status":{"status":"activated"},"twapId":102}]"#;
        assert!(!progress_from(history, "[]", 102).unwrap().running);
        assert!(running_from(history).is_empty());
        let reversed = r#"[{"time":1700000300,"state":{"coin":"ETH"},"status":{"status":"activated"},"twapId":102},{"time":1700000300,"state":{"coin":"ETH"},"status":{"status":"terminated"},"twapId":102}]"#;
        assert!(!progress_from(reversed, "[]", 102).unwrap().running);
        assert!(running_from(reversed).is_empty());
    }

    #[test]
    fn finds_the_twaps_still_running() {
        assert_eq!(running_from(HISTORY), vec![(1, String::new())]);
        let both = r#"[{"time":2,"state":{"coin":"ETH"},"status":{"status":"terminated"},"twapId":3},{"time":1,"state":{"coin":"ETH"},"status":{"status":"activated"},"twapId":3},{"time":4,"state":{"coin":"BTC"},"status":{"status":"activated"},"twapId":4}]"#;
        assert_eq!(running_from(both), vec![(4, "BTC".to_string())]);
    }

    #[test]
    fn only_a_gtc_entry_is_tracked() {
        assert!(entry_is_tracked(2));
        assert!(entry_is_tracked(3));
        assert!(entry_is_tracked(2 + 8));
        for t in [0, 1, 4, 8, 9] {
            assert!(!entry_is_tracked(t), "type {t}");
        }
    }

    #[test]
    fn minutes_stay_within_the_exchange_limits() {
        assert_eq!(set_minutes(4), None);
        assert_eq!(set_minutes(1441), None);
        assert_eq!(set_minutes(-5), None);
        assert_eq!(set_minutes(5), Some(5));
        assert_eq!(minutes(), 5);
        assert_eq!(set_minutes(0), Some(0), "0 switches TWAP entries off");
        assert_eq!(minutes(), 0);
    }
}
