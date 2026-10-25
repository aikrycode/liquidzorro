// Hyperliquid Buy/Sell Demo (C++ strategy DLL) //////////////////////////
// Example strategy for the liquidzorro Hyperliquid broker plugin.
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
//   - Run in [Trade] mode -- Test/Train/etc. are refused (see the
//     !is(TRADEMODE) check below)
//
// Build: Zorro64 has no Lite-C compiler and (without a Zorro S license)
// can't compile .cpp strategies on the fly either -- it can only load an
// already-compiled Release-mode .dll. So this file must be compiled
// externally, then only the resulting .dll (not this .cpp) goes in
// Strategy\:
//   copy this file into <ZorroFolder>\Strategy\, then from that folder:
//   Source\VC++\compile64.bat "Strategy\HyperliquidBuySellDemo+.cpp" ^
//     "Strategy\HyperliquidBuySellDemo.dll" "<path to VC\Auxiliary\Build>"
// This is unrelated to the liquidzorro.dll broker plugin itself, which is
// a normal Cargo/Rust build (see the top-level README).
#include <zorro.h>

#define HL_SYMBOL         "BTC/USD"
#define USD_AMOUNT        25     // how much to buy, in USDC notional

// Must match bridge::AMOUNT_SCALE in the plugin, and AssetsHyperliquid.csv's
// LotAmount column (0.00001 = 1/AMOUNT_SCALE): BrokerBuy2's Amount is a
// plain C int, so it can't represent a fractional BTC quantity directly --
// 1 Lot here is 1/AMOUNT_SCALE of one base-asset unit.
#define AMOUNT_SCALE      100000.

var Lots1, Lots2; // the two half-sized sell chunks, set after the buy fills
int Step;          // 0 = not started, 1 = bought, 2 = sold first half, 3 = done

DLLFUNC void run()
{
	BarPeriod = 1;
	LookBack = 0;
	TickTime = 1000;
	set(LOGFILE);

	if(!is(TRADEMODE)) {
		printf("\nUnsupported function: this demo only runs in [Trade] mode (not Test/Train/etc).");
		quit(0);
		return;
	}

	if(is(INITRUN)) {
		assetList("AssetsHyperliquid");
		Step = 0;
	}
	asset(HL_SYMBOL);
	if(is(LOOKBACK)) return;

	if(Step == 0) {
		var Price = priceClose(0); // zorro.h has no zero-arg inline wrapper for this one
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
