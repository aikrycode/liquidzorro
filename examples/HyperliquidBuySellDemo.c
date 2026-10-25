// Hyperliquid Buy/Sell Demo ////////////////////////////////////////////
// Example Lite-C script for the liquidzorro Hyperliquid broker plugin.
//
// Demonstrates the full order round trip through BrokerBuy2/BrokerTrade:
//   1. Buy ~USD_AMOUNT worth of the asset (one order)
//   2. Sell half of the filled position (one order)
//   3. Sell the remaining half (one order)
//
// Setup in Zorro:
//   - Broker dropdown: select "Hyperliquid"
//   - User field: leave blank
//   - Password field: your Hyperliquid private key (0x...)
//   - Demo/Real toggle: Demo = testnet, Real = mainnet -- start with Demo
//   - Run this script in [Trade] mode (it does nothing in [Test]/backtest)
//
// IMPORTANT: HL_BROKER_SYMBOL is verified against TESTNET's spot_meta() as of
// writing (via src/bridge/tests.rs's lists_spot_pairs) -- Hyperliquid only
// gives a friendly "TOKEN/USDC" name to a handful of "canonical" listings
// (e.g. PURR/USDC); every other spot pair, BTC/USDC included, is only
// addressable by its literal "@<index>" string. On testnet that's "@50".
// This index is almost certainly DIFFERENT on mainnet (testnet is full of
// junk test-token listings with unrelated sequential indices) -- re-run
// lists_spot_pairs against mainnet (BaseUrl::Mainnet) before ever using
// the Real toggle, and update this constant accordingly.
#define HL_SYMBOL         "BTC/USD"    // script-facing asset name (your choice)
#define HL_BROKER_SYMBOL  "@50"        // testnet BTC/USDC; re-verify for mainnet
#define USD_AMOUNT        25           // how much to buy, in USDC notional

// Must match bridge::AMOUNT_SCALE in the plugin: BrokerBuy2's Amount is a
// plain C int, so it can't represent a fractional BTC quantity directly.
// 1 Lot here = 1/AMOUNT_SCALE of one base-asset unit; vLotAmount=1 in
// assetAdd below means Lots passes straight through as that sub-unit count.
#define AMOUNT_SCALE      100000.

var Lots1, Lots2; // the two half-sized sell chunks, set after the buy fills
int Step;          // 0 = not started, 1 = bought, 2 = sold first half, 3 = done

function run()
{
	BarPeriod = 1;
	LookBack = 0;
	TickTime = 1000;
	set(LOGFILE);

	if(is(TESTMODE)) {
		quit("This demo trades live only -- run it in [Trade] mode.");
		return;
	}

	if(is(INITRUN)) {
		// vPrice=0 lets BrokerAsset supply the live price instead of a
		// hardcoded guess; vLotAmount=1 is the AMOUNT_SCALE convention above.
		assetAdd(HL_SYMBOL, 0,0,0,0, 1,1,100,1, 1, 0, HL_BROKER_SYMBOL);
		Step = 0;
	}
	asset(HL_SYMBOL);
	if(is(LOOKBACK)) return;

	if(Step == 0) {
		var Price = priceClose();
		if(Price <= 0) return; // wait for a live price
		Lots = ceil(USD_AMOUNT/Price*AMOUNT_SCALE);
		enterLong();
		printf("\nBuy submitted: %.0f lots (~$%d of %s)",Lots,USD_AMOUNT,HL_SYMBOL);
		Step = 1;
		return;
	}

	var OpenLots = 0;
	for(open_trades) if(TradeIsOpen) OpenLots += TradeLots;

	if(Step == 1) {
		if(OpenLots > 0) {
			Lots1 = floor(OpenLots/2);
			Lots2 = OpenLots - Lots1; // remainder, in case of an odd lot count
			printf("\nFilled %.0f lots total; will sell %.0f then %.0f",OpenLots,Lots1,Lots2);
			Step = 2;
		}
		return;
	}

	if(Step == 2) {
		exitLong(0,0,Lots1);
		printf("\nSold first half: %.0f lots",Lots1);
		Step = 3;
		return;
	}

	if(Step == 3) {
		exitLong(0,0,Lots2);
		printf("\nSold second half: %.0f lots",Lots2);
		Step = 4;
		return;
	}

	if(Step == 4) {
		if(once(1)) printf("\nDemo complete -- position fully closed.");
	}
}
