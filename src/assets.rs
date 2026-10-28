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

/// Rounds a price to 5 significant figures and the asset's decimal cap,
/// which is what Hyperliquid enforces.
pub fn round_price(value: f64, asset: &AssetInfo) -> f64 {
    round_to_significant_and_decimal(value, 5, asset.max_price_decimals())
}

pub fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn round_to_significant_and_decimal(value: f64, sig_figs: u32, max_decimals: u32) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let abs_value = value.abs();
    let magnitude = abs_value.log10().floor() as i32;
    let scale = 10f64.powi(sig_figs as i32 - magnitude - 1);
    let rounded = (abs_value * scale).round() / scale;
    round_to_decimals(rounded.copysign(value), max_decimals)
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn prices_round_to_five_significant_figures() {
        // a price carrying six significant figures is trimmed to five
        assert_eq!(round_price(92.41500, &spot(2)), 92.415);
        assert_eq!(round_price(92.42224, &spot(2)), 92.422);
        // same value on a perp is capped tighter
        assert_eq!(round_price(92.42224, &perp(2)), 92.422);
        assert_eq!(round_price(1234.567, &perp(2)), 1234.6);
    }

    #[test]
    fn rounding_handles_zero_and_negatives() {
        assert_eq!(round_price(0.0, &perp(2)), 0.0);
        assert_eq!(round_price(-92.42224, &spot(2)), -92.422);
    }
}
