//! Resolves a Zorro symbol to its Hyperliquid universe (perp or spot) and
//! the rounding rules that follow from it.
//!
//! The two universes share one symbol namespace in Zorro but differ in ways
//! that silently corrupt orders if ignored -- most importantly price
//! precision, which Hyperliquid derives from the asset's index: perps are
//! indexed below 10000 and allow 6 decimals before `sz_decimals` is
//! subtracted; spot assets are indexed at `10000 + spot_index` and allow 8.
//! An over-precise price is rejected outright as invalid.

use hyperliquid_rust_sdk::InfoClient;

/// Which Hyperliquid universe a symbol belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Universe {
    Perp,
    Spot,
}

#[derive(Clone, Debug)]
pub struct AssetInfo {
    pub universe: Universe,
    pub sz_decimals: u32,
    /// Highest leverage Hyperliquid allows on this asset; 1 for spot, which
    /// is fully collateralized.
    pub max_leverage: u32,
}

impl AssetInfo {
    pub fn is_perp(&self) -> bool {
        self.universe == Universe::Perp
    }

    /// Decimal places Hyperliquid accepts in a price for this asset. Mirrors
    /// the SDK's own index-based rule; getting it wrong means every order is
    /// rejected as an invalid price.
    pub fn max_price_decimals(&self) -> u32 {
        let max_decimals: u32 = match self.universe {
            Universe::Perp => 6,
            Universe::Spot => 8,
        };
        max_decimals.saturating_sub(self.sz_decimals)
    }

    /// The finest price step Hyperliquid accepts on this asset, reported to
    /// Zorro as its PIP. Zorro rounds the limit prices it sends to the PIP,
    /// so anything coarser moves orders: with a PIP of 1 a 2.7477 limit went
    /// out at 3, and one under 50 cents would go out at 0. Five significant
    /// figures can make the valid step coarser than this at a given price;
    /// the plugin's own rounding takes care of that.
    pub fn pip(&self) -> f64 {
        10f64.powi(-(self.max_price_decimals() as i32))
    }

    /// Coins in one Zorro lot. Zorro counts an order in lots with a 32-bit
    /// int, so the lot has to be small enough for fine sizes and large
    /// enough that a cheap coin's order still fits. An asset traded in
    /// fractions gets 1/AMOUNT_SCALE of a coin (21,474 coins at most per
    /// order); one traded only in whole coins -- every coin priced under a
    /// cent, at the time of writing -- gets one coin, since a smaller lot
    /// buys no precision there and $60 of it can be millions of coins.
    /// Must match the asset list's LotAmount, which Zorro sends before
    /// every order (SET_AMOUNT) and the plugin checks.
    pub fn lot_size(&self) -> f64 {
        if self.sz_decimals == 0 {
            1.0
        } else {
            1.0 / crate::bridge::AMOUNT_SCALE
        }
    }
}

/// Looks `symbol` up in the perp universe first, then spot. Perp names are
/// plain coins (`BTC`); spot pairs are either canonical (`PURR/USDC`) or
/// `@<index>`.
pub async fn fetch(info: &InfoClient, symbol: &str) -> Result<AssetInfo, String> {
    let meta = info
        .meta()
        .await
        .map_err(|e| format!("meta failed: {e}"))?;
    if let Some(asset) = meta.universe.iter().find(|a| a.name == symbol) {
        return Ok(AssetInfo {
            universe: Universe::Perp,
            sz_decimals: asset.sz_decimals,
            max_leverage: asset.max_leverage as u32,
        });
    }

    let spot_meta = info
        .spot_meta()
        .await
        .map_err(|e| format!("spot_meta failed: {e}"))?;
    let asset = spot_meta
        .universe
        .iter()
        .find(|a| a.name == symbol)
        .ok_or_else(|| format!("{symbol} not found in the perp or spot universe"))?;
    let base_token = spot_meta
        .tokens
        .iter()
        .find(|t| t.index == asset.tokens[0])
        .ok_or_else(|| format!("base token for {symbol} not found"))?;

    Ok(AssetInfo {
        universe: Universe::Spot,
        sz_decimals: base_token.sz_decimals as u32,
        max_leverage: 1,
    })
}

/// Rounds a size to the asset's allowed precision.
pub fn round_size(value: f64, sz_decimals: u32) -> f64 {
    round_to_decimals(value, sz_decimals)
}

/// Rounds a size *down* to the asset's step. For closing orders only, where
/// rounding up would ask to sell more than is held.
pub fn floor_size(value: f64, sz_decimals: u32) -> f64 {
    let factor = 10f64.powi(sz_decimals as i32);
    // Nudge first, so a value that is one float epsilon under a step
    // boundary is not thrown a whole step away.
    ((value * factor) + 1e-9).floor() / factor
}

/// Smallest order value Hyperliquid accepts, in USDC. Spot has no
/// reduce-only exemption, so a spot close under this is refused like any
/// other order.
pub const MIN_ORDER_USD: f64 = 10.0;

/// How much of a spot close can actually be sold: what Zorro asked for,
/// capped at the balance and floored to a whole step.
///
/// Closing a whole trade may round *up* by part of a step when the balance
/// holds it. The step floor otherwise strands a sliver on every trade,
/// slivers from successive trades pile up, and none of them can ever be
/// sold on its own. Letting a closing trade take up to one step from the
/// balance keeps the leftover under one step instead of growing.
pub fn spot_close_size(requested: f64, held: f64, sz_decimals: u32, closes_whole_trade: bool) -> f64 {
    let target = if closes_whole_trade { ceil_size(requested, sz_decimals) } else { requested };
    floor_size(target.min(held).max(0.0), sz_decimals)
}

/// Whether an order of `sz` at `price` clears the exchange minimum.
pub fn meets_minimum(sz: f64, price: f64) -> bool {
    sz > 0.0 && sz * price >= MIN_ORDER_USD
}

/// Rounds a size *up* to the asset's step, ignoring float dust just above
/// a step boundary.
fn ceil_size(value: f64, sz_decimals: u32) -> f64 {
    let factor = 10f64.powi(sz_decimals as i32);
    ((value * factor) - 1e-9).ceil() / factor
}

/// Which way a price is rounded to a valid one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding {
    Nearest,
    Down,
    Up,
}

impl Rounding {
    /// Away from the other side of the book: a buy down, a sell up. For a
    /// limit price, so rounding never makes an order cross the book -- a
    /// post-only buy rounded up into the ask is refused.
    pub fn passive(is_buy: bool) -> Self {
        if is_buy { Rounding::Down } else { Rounding::Up }
    }

    /// Toward the other side of the book: a buy up, a sell down. For a
    /// price meant to trade at once, so rounding never makes it miss.
    pub fn aggressive(is_buy: bool) -> Self {
        if is_buy { Rounding::Up } else { Rounding::Down }
    }
}

/// Rounds a price to one Hyperliquid accepts: at most 5 significant figures
/// and the asset's decimal cap, except that a whole number is always valid.
/// So a price with 5 or more digits before the point is rounded to a whole
/// number, not cut to 5 figures (103457.3 becomes 103457, not 103460).
pub fn round_price(value: f64, asset: &AssetInfo, rounding: Rounding) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let magnitude = value.abs().log10().floor() as i32;
    let decimals = (4 - magnitude).clamp(0, asset.max_price_decimals() as i32);
    let factor = 10f64.powi(decimals);
    let scaled = value * factor;
    // Nudged like the size helpers, so a price already on a step is kept
    // and not thrown a whole step away by float error.
    let steps = match rounding {
        Rounding::Nearest => scaled.round(),
        Rounding::Down => (scaled + 1e-9).floor(),
        Rounding::Up => (scaled - 1e-9).ceil(),
    };
    // Dividing gives the exact decimal value; multiplying by 10^-n would not.
    steps / factor
}

pub fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {

    /// The live failure: a 3-coin spot buy pays its fee in the base asset,
    /// leaving 2.9979. Rounding that to 3 asked to sell more than the
    /// balance held, so the exchange refused and the position stayed open.
    #[test]
    fn a_close_never_rounds_up_past_what_is_held() {
        assert_eq!(super::floor_size(2.9979, 0), 2.0);
        assert_eq!(super::floor_size(0.9979, 0), 0.0);
        assert_eq!(super::round_size(2.9979, 0), 3.0, "rounding is what broke it");
    }

    #[test]
    fn flooring_keeps_whole_steps_intact() {
        assert_eq!(super::floor_size(3.0, 0), 3.0);
        assert_eq!(super::floor_size(0.28, 2), 0.28);
        assert_eq!(super::floor_size(0.2849, 2), 0.28);
    }

    /// A size landing an epsilon under a step boundary must not lose a whole
    /// step -- on a whole-coin asset that would be the entire order.
    #[test]
    fn flooring_tolerates_float_dust_below_a_step() {
        assert_eq!(super::floor_size(3.0 - 1e-12, 0), 3.0);
        assert_eq!(super::floor_size(0.28 - 1e-12, 2), 0.28);
    }

    use super::*;

    fn perp(sz: u32) -> AssetInfo {
        AssetInfo { universe: Universe::Perp, sz_decimals: sz, max_leverage: 50 }
    }
    fn spot(sz: u32) -> AssetInfo {
        AssetInfo { universe: Universe::Spot, sz_decimals: sz, max_leverage: 1 }
    }

    #[test]
    fn price_decimals_differ_between_universes() {
        // the rule that makes a perp order valid or rejected
        assert_eq!(perp(2).max_price_decimals(), 4);
        assert_eq!(spot(2).max_price_decimals(), 6);
        assert_eq!(perp(0).max_price_decimals(), 6);
        assert_eq!(spot(0).max_price_decimals(), 8);
    }

    #[test]
    fn price_decimals_never_underflow() {
        // sz_decimals above the cap must clamp, not wrap around
        assert_eq!(perp(8).max_price_decimals(), 0);
    }

    use Rounding::{Down, Nearest, Up};

    #[test]
    fn prices_round_to_five_significant_figures() {
        // a price carrying six significant figures is trimmed to five
        assert_eq!(round_price(92.41500, &spot(2), Nearest), 92.415);
        assert_eq!(round_price(92.42224, &spot(2), Nearest), 92.422);
        // same value on a perp is capped tighter
        assert_eq!(round_price(92.42224, &perp(2), Nearest), 92.422);
        assert_eq!(round_price(1234.567, &perp(2), Nearest), 1234.6);
    }

    /// The live case: a 2.7477 PURR limit is already valid and must go out
    /// unchanged, whichever way it would round.
    #[test]
    fn a_valid_price_is_kept_whichever_way_it_rounds() {
        for r in [Nearest, Down, Up] {
            assert_eq!(round_price(2.7477, &spot(0), r), 2.7477);
            assert_eq!(round_price(0.0042, &perp(0), r), 0.0042);
            assert_eq!(round_price(3000.1, &perp(4), r), 3000.1);
        }
    }

    /// A limit buy at 4.57955 rounded to the nearest step went up to 4.5796
    /// and could cross the ask; rounded passively it rests at 4.5795.
    #[test]
    fn limit_prices_round_away_from_the_book() {
        assert_eq!(round_price(4.57955, &spot(0), Rounding::passive(true)), 4.5795);
        assert_eq!(round_price(4.57955, &spot(0), Rounding::passive(false)), 4.5796);
        assert_eq!(round_price(4.57951, &spot(0), Down), 4.5795);
        assert_eq!(round_price(4.57951, &spot(0), Up), 4.5796);
    }

    #[test]
    fn marketable_prices_round_toward_the_book() {
        assert_eq!(round_price(4.57951, &spot(0), Rounding::aggressive(true)), 4.5796);
        assert_eq!(round_price(4.57959, &spot(0), Rounding::aggressive(false)), 4.5795);
    }

    /// Hyperliquid accepts any whole number, so a BTC limit keeps its
    /// dollars instead of being cut to every $10.
    #[test]
    fn prices_of_five_digits_and_more_round_to_whole_numbers() {
        assert_eq!(round_price(103457.3, &perp(5), Nearest), 103457.0);
        assert_eq!(round_price(103457.3, &perp(5), Up), 103458.0);
        assert_eq!(round_price(103457.0, &perp(5), Down), 103457.0);
        assert_eq!(round_price(12345.6, &perp(5), Down), 12345.0);
        // four digits still keep one decimal (5 significant figures)
        assert_eq!(round_price(1234.56, &perp(4), Down), 1234.5);
    }

    /// The decimal cap still applies under 5 significant figures: a perp
    /// with 4 size decimals allows only 2 price decimals.
    #[test]
    fn the_decimal_cap_still_applies() {
        assert_eq!(round_price(1.23456, &perp(4), Nearest), 1.23);
        assert_eq!(round_price(1.23456, &perp(4), Up), 1.24);
        assert_eq!(round_price(0.00123456, &perp(0), Down), 0.001234);
    }

    #[test]
    fn the_pip_is_the_finest_step_the_exchange_accepts() {
        assert_eq!(perp(4).pip(), 0.01); // ETH
        assert_eq!(perp(5).pip(), 0.1); // BTC
        assert_eq!(perp(0).pip(), 0.000001); // kPEPE
        assert_eq!(spot(0).pip(), 0.00000001); // PURR
        assert_eq!(spot(2).pip(), 0.000001);
        // any price rounded the plugin's way is a whole number of pips
        for (px, a) in [(2.7477, spot(0)), (0.004213, perp(0)), (3012.37, perp(4))] {
            let steps = round_price(px, &a, Nearest) / a.pip();
            assert!((steps - steps.round()).abs() < 1e-6, "{px}: {steps}");
        }
    }

    /// Zorro derives both trade profit and Leverage from PipCost/Pip, and
    /// that ratio has to equal the lot size. With PipCost 1 and Pip 1 it
    /// read 1 instead of 1/AMOUNT_SCALE, so Zorro computed leverage
    /// price/MarginCost = 100000 and sent that back via SET_LEVERAGE.
    #[test]
    fn pip_ratio_matches_lot_size() {
        for asset in [perp(4), perp(0), spot(0), spot(2)] {
            let pip = asset.pip();
            let pip_cost = pip * asset.lot_size(); // what BrokerAsset reports
            assert!((pip_cost / pip - asset.lot_size()).abs() < 1e-15);

            // leverage Zorro infers, for a 1x lot: price/MarginCost * PipCost/Pip
            let price = 2374.0;
            let margin_cost = price * asset.lot_size();
            let leverage = price / margin_cost * (pip_cost / pip);
            assert!((leverage - 1.0).abs() < 1e-9, "expected 1x, got {leverage}");
        }
    }

    /// Whole-coin assets count lots in coins, so a cheap coin's order fits
    /// Zorro's 32-bit lot count; the rest keep the fine default.
    #[test]
    fn a_whole_coin_asset_counts_lots_in_coins() {
        assert_eq!(perp(0).lot_size(), 1.0);
        assert_eq!(spot(0).lot_size(), 1.0);
        assert_eq!(perp(4).lot_size(), 0.00001);
        assert_eq!(spot(2).lot_size(), 0.00001);
        // $60 of a coin at $0.00014: about 430,000 coins
        let coins = 60.0 / 0.00014;
        assert!(coins / perp(0).lot_size() < i32::MAX as f64);
        assert!(coins / perp(4).lot_size() > i32::MAX as f64, "what overflowed before");
    }

    #[test]
    fn rounding_handles_zero_and_negatives() {
        assert_eq!(round_price(0.0, &perp(2), Nearest), 0.0);
        assert_eq!(round_price(-92.42224, &spot(2), Nearest), -92.422);
    }

    /// The second live failure: chunks that floored under $10 were
    /// refused, and a trade's last sliver could never be sold at all.
    #[test]
    fn a_partial_close_sells_whole_steps_it_holds() {
        assert_eq!(spot_close_size(2.9979, 9.993, 0, false), 2.0);
        assert_eq!(spot_close_size(4.0, 9.993, 0, false), 4.0);
    }

    #[test]
    fn a_close_never_sells_more_than_the_balance() {
        assert_eq!(spot_close_size(10.0, 9.993, 0, false), 9.0);
        assert_eq!(spot_close_size(10.0, 9.993, 0, true), 9.0);
        assert_eq!(spot_close_size(1.0, 0.0, 0, true), 0.0);
    }

    /// 2.993 left in the trade and 3.9 in the wallet (old dust included):
    /// selling 3 retires the trade; selling 2 would strand 0.993 forever.
    #[test]
    fn closing_a_whole_trade_may_take_one_step_from_the_balance() {
        assert_eq!(spot_close_size(2.993, 3.9, 0, true), 3.0);
        assert_eq!(spot_close_size(2.993, 3.9, 0, false), 2.0);
        // never more than one step beyond the trade
        assert_eq!(spot_close_size(2.0, 50.0, 0, true), 2.0);
        assert_eq!(spot_close_size(2.0000000001, 50.0, 0, true), 2.0);
    }

    #[test]
    fn the_ten_dollar_minimum_is_inclusive() {
        assert!(meets_minimum(3.0, 4.53));
        assert!(!meets_minimum(2.0, 4.53));
        assert!(meets_minimum(1.0, 10.0));
        assert!(!meets_minimum(0.0, 1e9));
    }
}
