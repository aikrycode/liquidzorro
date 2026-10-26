// Hyperliquid Buy/Sell Demo (C++ strategy DLL) ///////////////////////////
// Buys ~USD_AMOUNT of HL_SYMBOL, sells it in two halves, sweeps any
// above-dust leftover. Broker: Hyperliquid. User: blank. Password: your
// private key. Demo = testnet, Real = mainnet. [Trade] mode only.
//
// Zorro64 has no Lite-C compiler and can't compile .cpp on the fly without
// Zorro S -- compile externally, deploy only the .dll:
//   Source\VC++\compile64.bat "Strategy\HyperliquidBuySellDemo+.cpp" ^
//     "Strategy\HyperliquidBuySellDemo.dll" "<path to VC\Auxiliary\Build>"
#include <zorro.h>

#define HL_SYMBOL         "HYPE/USDC"   // testnet symbol "@1035"
#define USD_AMOUNT        25            // USDC notional to buy

// Amount is a plain C int, can't hold a fraction of a base-asset unit, so
// 1 Lot = 1/AMOUNT_SCALE of one unit. Must match bridge::AMOUNT_SCALE and
// AssetsHyperliquid.csv's LotAmount column.
#define AMOUNT_SCALE      100000.

// Below this, a leftover position is too small to reliably sell (rounds to
// zero at the asset's real size precision and gets rejected) -- ignore it
// and just run the normal buy/sell/sell flow instead of chasing dust.
#define DUST_THRESHOLD_LOTS 1000.

var Lots1, Lots2;  // the two half-sized sell chunks
int Step;          // -1 undetermined, 0 not started, 1 bought, 2/3 sold
                    // halves, 4 sweep dust, 5 done
int Resuming;      // 1 if this position was found via GET_POSITION, not
                    // bought this session (changes how we close it)

DLLFUNC void run()
{
	BarPeriod = 1;
	LookBack = 0;
	TickTime = 1000;
	set(LOGFILE);
	set(NFA|OFF);   // else exitLong fails: "Error 075 - closing not supported"
	SaveMode = 0;   // avoid resuming a trade id our plugin no longer knows

	if(!is(TRADEMODE)) {
		printf("\nUnsupported function: this demo only runs in [Trade] mode.");
		quit(0);
		return;
	}

	if(is(INITRUN)) {
		assetList("AssetsHyperliquid");
		Step = -1;
	}
	asset(HL_SYMBOL);
	if(is(LOOKBACK)) return;

	// Real exchange state, not Zorro's local (process-only) trade list --
	// catches a leftover position across a full app restart too.
	if(Step == -1) {
		var ExistingLots = brokerCommand(GET_POSITION,SymbolTrade);
		if(ExistingLots > DUST_THRESHOLD_LOTS) {
			Lots1 = floor(ExistingLots/2);
			Lots2 = ExistingLots - Lots1;
			Resuming = 1;
			printf("\nResuming: existing position of %.0f lots -- selling instead of buying",ExistingLots);
			Step = 2;
		} else {
			if(ExistingLots > 0) printf("\nIgnoring dust position: %.0f lots",ExistingLots);
			Resuming = 0;
			Step = 0;
		}
	}

	if(Step == 0) {
		var Price = priceClose(0); // no zero-arg inline wrapper in C++
		printf("\n[debug] asset=%s price=%.6f",Asset,Price);
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
			Lots2 = OpenLots - Lots1;
			printf("\nFilled %.0f lots; selling %.0f then %.0f",OpenLots,Lots1,Lots2);
			Step = 2;
		}
		return;
	}

	// exitLong only closes an existing Zorro trade object; a resumed
	// position has none, so exitLong would silently no-op. enterShort
	// always places a real sell via BrokerBuy2 (Hyperliquid spot has no
	// real shorting, so to the plugin this is just "sell this much").
	// Neither call reports success here -- the plugin's own BrokerBuy2
	// OK/FAILED log line (printed by the DLL, not this script) is the
	// accurate record of what actually happened.
	if(Step == 2) {
		if(Resuming) { Lots = Lots1; enterShort(); } else exitLong(0,0,Lots1);
		printf("\nSell submitted: first half, %.0f lots",Lots1);
		Step = 3;
		return;
	}

	if(Step == 3) {
		if(Resuming) { Lots = Lots2; enterShort(); } else exitLong(0,0,Lots2);
		printf("\nSell submitted: second half, %.0f lots",Lots2);
		Step = 4;
		return;
	}

	// IOC sells can partially fill; sweep any leftover above dust once.
	if(Step == 4) {
		var Leftover = brokerCommand(GET_POSITION,SymbolTrade);
		if(Leftover > DUST_THRESHOLD_LOTS) {
			printf("\nSweeping leftover: %.0f lots",Leftover);
			Lots = Leftover;
			enterShort();
		} else if(Leftover > 0) {
			printf("\nIgnoring dust leftover: %.0f lots",Leftover);
		}
		Step = 5;
		return;
	}

	if(Step == 5) {
		if(once(1)) printf("\nDemo complete -- position fully closed.");
	}
}
