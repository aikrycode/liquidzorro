//! Writes a Zorro asset list from the exchange's own data (EXPORT_ASSETS).
//!
//! A hand-written asset list goes wrong in ways that are silent until an
//! order: a LotAmount that does not match the plugin's lot gets every entry
//! refused, and a coarse PIP moves every limit price. The exchange already
//! knows each asset's size and price steps, so the list is built from that,
//! for every perp and spot pair it lists, on the network logged in to.

use std::collections::{HashMap, HashSet};

use hyperliquid_rust_sdk::{AssetContext, AssetMeta, SpotMeta};
use serde_json::Value;

use crate::assets::{AssetInfo, Universe};
use crate::{runtime, state};

/// Zorro's own header, as it writes `Log\Assets.csv`.
const HEADER: &str = "Name,Price,Spread,RollLong,RollShort,PIP,PIPCost,MarginCost,Market,Multiplier,Commission,Symbol";

/// Zorro's limit on an asset name.
const MAX_NAME: usize = 15;

/// One asset list line.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    price: f64,
    spread: f64,
    roll_long: f64,
    roll_short: f64,
    pip: f64,
    pip_cost: f64,
    lot: f64,
    commission: f64,
    symbol: String,
}

impl Row {
    fn csv(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},0,{},{},{}",
            self.name,
            num(self.price),
            num(self.spread),
            num(self.roll_long),
            num(self.roll_short),
            num(self.pip),
            num(self.pip_cost),
            // 1x: margin is the whole position value. Zorro derives the
            // leverage it sends (SET_LEVERAGE) from this, so a list that
            // claimed the asset's maximum would trade at it.
            -100,
            num(self.lot),
            num(self.commission),
            self.symbol
        )
    }
}

/// `x` to 10 significant digits, without exponent or trailing zeros:
/// float arithmetic otherwise writes 0.14 as 0.13999999999999999.
fn num(x: f64) -> String {
    if x == 0.0 || !x.is_finite() {
        return "0".to_string();
    }
    let decimals = (9 - x.abs().log10().floor() as i32).clamp(0, 30) as usize;
    let s = format!("{x:.decimals$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { &s };
    if s == "-0" { "0".to_string() } else { s.to_string() }
}

/// Roundtrip taker fees as Zorro's Commission: percent of the trade value,
/// negative. `(perp, spot)`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Fees {
    perp: f64,
    spot: f64,
}

/// Reads `userCrossRate` and `userSpotCrossRate` (taker rates, as
/// fractions) from a `userFees` reply.
fn fees_from(reply: &str) -> Option<Fees> {
    let v: Value = serde_json::from_str(reply).ok()?;
    let rate = |key: &str| v.get(key)?.as_str()?.parse::<f64>().ok();
    let commission = |rate: f64| -(2.0 * rate * 100.0);
    Some(Fees {
        perp: commission(rate("userCrossRate")?),
        spot: commission(rate("userSpotCrossRate")?),
    })
}

/// A name Zorro accepts: letters, digits, '/', '_' and the '-' of
/// `ETH-PERP`, at most 15 characters, and not taken yet.
fn usable(name: &str, taken: &HashSet<String>) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-'))
        && !taken.contains(name)
}

/// The rows of every perp and spot pair that has a price. Perps are named
/// `COIN-PERP`, spot pairs `BASE/QUOTE`; a name Zorro cannot take, or one
/// already used, falls back to `PERP_<index>` / `SPOT_<index>`.
fn rows(
    perps: &[(AssetMeta, AssetContext)],
    spot: &SpotMeta,
    mids: &HashMap<String, String>,
    fees: Fees,
) -> (Vec<Row>, usize) {
    let mut out = Vec::new();
    let mut taken = HashSet::new();
    let mut skipped = 0;
    let mid = |symbol: &str| mids.get(symbol).and_then(|m| m.parse::<f64>().ok()).filter(|m| *m > 0.0);

    for (index, (meta, ctx)) in perps.iter().enumerate() {
        let Some(price) = mid(&meta.name) else {
            skipped += 1; // delisted: no market
            continue;
        };
        let info = AssetInfo {
            universe: Universe::Perp,
            sz_decimals: meta.sz_decimals,
            max_leverage: meta.max_leverage as u32,
        };
        let lot = info.lot_size();
        // The exchange's own estimate of what crossing the book costs.
        let spread = match ctx.impact_pxs.as_deref() {
            Some([bid, ask]) => match (bid.parse::<f64>(), ask.parse::<f64>()) {
                (Ok(bid), Ok(ask)) if ask > bid => ask - bid,
                _ => 0.0,
            },
            _ => 0.0,
        };
        let (roll_long, roll_short) = match (ctx.funding.parse::<f64>(), ctx.oracle_px.parse::<f64>()) {
            // + 0.0 turns a zero rate's -0 into 0
            (Ok(rate), Ok(oracle)) => {
                let (long, short) = super::roll_per_lot(rate, oracle, lot);
                (long + 0.0, short + 0.0)
            }
            _ => (0.0, 0.0),
        };
        let name = Some(format!("{}-PERP", meta.name))
            .filter(|n| usable(n, &taken))
            .unwrap_or_else(|| format!("PERP_{index}"));
        taken.insert(name.clone());
        out.push(Row {
            name,
            price,
            spread,
            roll_long,
            roll_short,
            pip: info.pip(),
            pip_cost: info.pip() * lot,
            lot,
            commission: fees.perp,
            symbol: meta.name.clone(),
        });
    }

    for pair in &spot.universe {
        let token = |i: usize| spot.tokens.iter().find(|t| t.index == pair.tokens[i]);
        let (Some(base), Some(quote), Some(price)) = (token(0), token(1), mid(&pair.name)) else {
            skipped += 1;
            continue;
        };
        let info = AssetInfo { universe: Universe::Spot, sz_decimals: base.sz_decimals as u32, max_leverage: 1 };
        let lot = info.lot_size();
        let name = Some(format!("{}/{}", base.name, quote.name))
            .filter(|n| usable(n, &taken))
            .unwrap_or_else(|| format!("SPOT_{}", pair.index));
        taken.insert(name.clone());
        out.push(Row {
            name,
            price,
            spread: 0.0,
            roll_long: 0.0,
            roll_short: 0.0,
            pip: info.pip(),
            pip_cost: info.pip() * lot,
            lot,
            commission: fees.spot,
            symbol: pair.name.clone(),
        });
    }
    (out, skipped)
}

fn csv(rows: &[Row]) -> String {
    let mut s = String::from(HEADER);
    s.push_str("\r\n");
    for row in rows {
        s.push_str(&row.csv());
        s.push_str("\r\n");
    }
    s
}

/// Writes the asset list to `path` (relative to Zorro's folder). Returns
/// the number of assets written.
pub fn export_assets(path: &str) -> Result<usize, String> {
    let plugin = state::get().ok_or("not logged in")?;
    let info = &plugin.info_client;
    let (meta, contexts, spot, mids, fees_reply) = runtime::get().block_on(async {
        let (meta, contexts) = info
            .meta_and_asset_contexts()
            .await
            .map_err(|e| format!("meta failed: {e}"))?;
        let spot = info.spot_meta().await.map_err(|e| format!("spot_meta failed: {e}"))?;
        let mids = info.all_mids().await.map_err(|e| format!("all_mids failed: {e}"))?;
        let body = format!(r#"{{"type":"userFees","user":"{:#x}"}}"#, plugin.address);
        let fees = info.http_client.post("/info", body).await.ok();
        Ok::<_, String>((meta, contexts, spot, mids, fees))
    })?;
    // Without the account's rates the list still holds; its backtests just
    // see no commission, and the log says so.
    let fees = fees_reply.as_deref().and_then(fees_from);
    if fees.is_none() {
        state::log_message("EXPORT_ASSETS: fee rates unavailable; Commission left at 0");
    }
    let perps: Vec<_> = meta.universe.into_iter().zip(contexts).collect();
    let (rows, skipped) = rows(&perps, &spot, &mids, fees.unwrap_or_default());
    if rows.is_empty() {
        return Err("the exchange listed no asset with a price".to_string());
    }
    std::fs::write(path, csv(&rows)).map_err(|e| format!("cannot write {path}: {e}"))?;
    let perp_count = rows.iter().filter(|r| r.name.ends_with("-PERP") || r.name.starts_with("PERP_")).count();
    state::log_message(&format!(
        "EXPORT_ASSETS: wrote {} assets ({perp_count} perps, {} spot) to {path}; skipped {skipped} without a price",
        rows.len(),
        rows.len() - perp_count
    ));
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perp(name: &str, sz: u32, funding: &str, impact: Option<[&str; 2]>) -> (AssetMeta, AssetContext) {
        let meta: AssetMeta = serde_json::from_value(serde_json::json!({
            "name": name, "szDecimals": sz, "maxLeverage": 25
        }))
        .unwrap();
        let ctx: AssetContext = serde_json::from_value(serde_json::json!({
            "dayNtlVlm": "0", "funding": funding, "impactPxs": impact.map(|p| p.to_vec()),
            "markPx": "2000", "midPx": "2000", "openInterest": "0", "oraclePx": "2000",
            "premium": "0", "prevDayPx": "2000"
        }))
        .unwrap();
        (meta, ctx)
    }

    fn spot_meta() -> SpotMeta {
        let token = |name: &str, index: usize, sz: u8| serde_json::json!({
            "name": name, "szDecimals": sz, "weiDecimals": 8, "index": index,
            "tokenId": "0x00000000000000000000000000000000", "isCanonical": true
        });
        serde_json::from_value(serde_json::json!({
            "universe": [
                {"tokens": [1, 0], "name": "PURR/USDC", "index": 0, "isCanonical": true},
                {"tokens": [2, 0], "name": "@7", "index": 7, "isCanonical": false},
                {"tokens": [3, 0], "name": "@8", "index": 8, "isCanonical": false},
                {"tokens": [2, 0], "name": "@9", "index": 9, "isCanonical": false},
                {"tokens": [4, 0], "name": "@10", "index": 10, "isCanonical": false}
            ],
            "tokens": [token("USDC", 0, 8), token("PURR", 1, 0), token("HYPE", 2, 2),
                       token("A VERY LONG NAME", 3, 2), token("QUIET", 4, 2)]
        }))
        .unwrap()
    }

    fn mids() -> HashMap<String, String> {
        [("ETH", "2000.5"), ("PURR/USDC", "4.5"), ("@7", "30"), ("@8", "1"), ("@9", "31")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const FEES: Fees = Fees { perp: -0.09, spot: -0.14 };

    #[test]
    fn a_perp_row_carries_the_exchange_steps() {
        let perps = [perp("ETH", 4, "0.0000125", Some(["1999.9", "2000.1"]))];
        let (rows, _) = rows(&perps, &SpotMeta { universe: vec![], tokens: vec![] }, &mids(), FEES);
        let eth = &rows[0];
        assert_eq!(eth.name, "ETH-PERP");
        assert_eq!(eth.symbol, "ETH");
        assert_eq!(eth.price, 2000.5);
        assert!((eth.spread - 0.2).abs() < 1e-9);
        assert_eq!(eth.pip, 0.01);
        assert_eq!(eth.lot, 0.00001);
        assert!((eth.pip_cost - 0.0000001).abs() < 1e-18);
        assert!(eth.roll_long < 0.0 && eth.roll_short > 0.0, "a positive rate charges longs");
        assert_eq!(eth.commission, -0.09);
        assert_eq!(
            eth.csv(),
            format!("ETH-PERP,2000.5,0.2,{},{},0.01,0.0000001,-100,0,0.00001,-0.09,ETH",
                num(eth.roll_long), num(eth.roll_short))
        );
    }

    #[test]
    fn spot_pairs_are_named_by_their_tokens() {
        let (rows, skipped) = rows(&[], &spot_meta(), &mids(), FEES);
        let names: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.symbol.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("PURR/USDC", "PURR/USDC"),
                ("HYPE/USDC", "@7"),
                ("SPOT_8", "@8"), // the token's name has blanks and is too long
                ("SPOT_9", "@9"), // HYPE/USDC is taken
            ]
        );
        assert_eq!(skipped, 1, "@10 has no price");
        let purr = &rows[0];
        assert_eq!((purr.lot, purr.pip, purr.pip_cost), (1.0, 0.00000001, 0.00000001));
        let hype = &rows[1];
        assert_eq!((hype.lot, hype.pip), (0.00001, 0.000001));
        assert_eq!((hype.roll_long, hype.roll_short, hype.commission), (0.0, 0.0, -0.14));
    }

    #[test]
    fn a_perp_without_a_price_is_skipped() {
        let perps = [perp("GONE", 2, "0", None), perp("ETH", 4, "0", None)];
        let (rows, skipped) = rows(&perps, &SpotMeta { universe: vec![], tokens: vec![] }, &mids(), FEES);
        assert_eq!(rows.len(), 1);
        assert_eq!(skipped, 1);
        assert_eq!(rows[0].spread, 0.0, "no impact prices, no spread");
    }

    #[test]
    fn fees_are_roundtrip_percent_and_negative() {
        let reply = r#"{"userCrossRate":"0.00045","userAddRate":"0.00015","userSpotCrossRate":"0.0007"}"#;
        let fees = fees_from(reply).unwrap();
        assert!((fees.perp + 0.09).abs() < 1e-12 && (fees.spot + 0.14).abs() < 1e-12);
        assert_eq!(fees_from("{}"), None);
    }

    #[test]
    fn numbers_are_written_without_float_noise() {
        assert_eq!(num(-2.0 * 0.0007 * 100.0), "-0.14");
        assert_eq!(num(0.01 * 0.00001), "0.0000001");
        assert_eq!(num(1803.65), "1803.65");
        assert_eq!(num(10.160000000000082), "10.16");
        assert_eq!(num(0.000000000010000000000000001), "0.00000000001");
        assert_eq!(num(90000.0), "90000");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(0.0000025598688714000004), "0.000002559868871");
    }

    #[test]
    fn the_file_has_zorros_header_and_crlf_lines() {
        let (rows, _) = rows(&[], &spot_meta(), &mids(), FEES);
        let text = csv(&rows);
        assert!(text.starts_with("Name,Price,Spread,RollLong,RollShort,PIP,PIPCost,MarginCost,Market,Multiplier,Commission,Symbol\r\n"));
        assert!(text.contains("\r\nPURR/USDC,4.5,0,0,0,0.00000001,0.00000001,-100,0,1,-0.14,PURR/USDC\r\n"));
    }
}
