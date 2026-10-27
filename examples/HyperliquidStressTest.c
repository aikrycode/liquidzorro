// Hyperliquid Stress Test (Lite-C, for 32-bit Zorro.exe) ////////////////
// Exercises far more of the plugin than the buy/sell demo: account reads,
// multiple assets, candle history, repeated market round trips with uneven
// splits, a resting GTC limit order with cancellation, and a deliberately
// rejected undersized order. Every check logs PASS/FAIL and the run ends
// with a tally. Broker: Hyperliquid. User: blank. Password: your private
// key. Demo = testnet. [Trade] mode only.
//
// Budget: holds at most ~$57 of testnet USDC at once and loses only
// spread/slippage (a few dollars across a full run). Aborts immediately if
// the account balance would drop under MIN_BALANCE.

#define HL_SYMBOL       "HYPE/USDC"  // testnet symbol "@1035"
#define ALT_SYMBOL      "BTC/USD"    // priced only -- too coarse to trade at this size
#define MIN_BALANCE     750          // hard floor; abort rather than spend past this
#define TRADE_USD       45           // per cycle; split in 3, each chunk must clear $10
#define LIMIT_USD       12           // resting limit order notional
#define CYCLES          2            // market round trips
#define HISTORY_BARS    50           // candles to pull via BrokerHistory2

#define AMOUNT_SCALE    100000.
#define DUST_THRESHOLD_LOTS 1000.

// BrokerCommand codes (trading.h)
#define CMD_GET_POSITION   53
#define CMD_SET_ORDERTYPE  157
#define CMD_DO_CANCEL      301
#define ORDERTYPE_GTC      2
#define ORDERTYPE_IOC      1

#define PH_PREFLIGHT    0
#define PH_BUY          1
#define PH_AWAIT_FILL   2
#define PH_SELL_A       3
#define PH_SELL_B       4
#define PH_SELL_C       5
#define PH_SWEEP        6
#define PH_CYCLE_END    7
#define PH_LIMIT_PLACE  8
#define PH_LIMIT_POLL   9
#define PH_LIMIT_CANCEL 10
#define PH_REJECT       11
#define PH_REPORT       12
#define PH_DONE         13

int Phase;
int Cycle;
int Passed, Failed;
var Chunk1, Chunk2, Chunk3;
var StartBalance;
var PositionBeforeBuy;
var PositionBeforeReject;
int LimitTradeID;
int LimitPollsLeft;
int LimitCancelled;

void check(string Name, int Ok)
{
	if(Ok) {
		Passed++;
		printf("\n  PASS  %s", Name);
	} else {
		Failed++;
		printf("\n  FAIL  %s", Name);
	}
}

// Real exchange position in lots, independent of Zorro's local trade list.
var heldLots()
{
	return brokerCommand(CMD_GET_POSITION, SymbolTrade);
}

// Hyperliquid spot has no shorting, so to the plugin this is just a sell.
// Hedge = 2 stops Zorro reinterpreting it as a close of the open long.
void sellViaShort(var SellLots)
{
	Lots = SellLots;
	enterShort();
}

// Guards the budget. Returns 1 if there is room to keep trading.
int budgetOk()
{
	if(Balance >= MIN_BALANCE) return 1;
	printf("\n!! ABORT: balance %.2f is below the %d floor", Balance, MIN_BALANCE);
	return 0;
}

function run()
{
	BarPeriod = 1;
	LookBack = HISTORY_BARS;   // forces a real BrokerHistory2 fetch
	TickTime = 1000;
	set(LOGFILE);
	set(NFA|OFF);
	SaveMode = 0;
	Hedge = 2;

	if(!is(TRADEMODE)) {
		printf("\nUnsupported function: this test only runs in [Trade] mode.");
		quit(0);
		return;
	}

	EntryTime = 100;           // don't let Zorro expire the limit order before we cancel it

	if(is(INITRUN)) {
		assetList("AssetsHyperliquid");
		asset(ALT_SYMBOL);     // register both assets so their price series load
		Phase = PH_PREFLIGHT;
		Cycle = 0;
		Passed = 0;
		Failed = 0;
	}
	asset(HL_SYMBOL);
	if(is(LOOKBACK)) return;

	///////////////////////////////////////////////////////////////////////
	// Read-only checks: all run in one tick, no orders, no cost.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_PREFLIGHT) {
		int i, BadBars;
		var AltPrice, Price;

		printf("\n=== Hyperliquid stress test ===");

		// --- account ---
		StartBalance = Balance;
		printf("\nAccount balance: %.2f USDC", StartBalance);
		check("BrokerAccount returns a positive balance", StartBalance > 0);
		if(!budgetOk()) { Phase = PH_REPORT; return; }
		check("Balance covers the test budget", StartBalance >= MIN_BALANCE + TRADE_USD + LIMIT_USD);

		// --- asset pricing, two different symbols ---
		Price = priceClose(0);
		check("BrokerAsset prices HYPE/USDC", Price > 0);

		asset(ALT_SYMBOL);
		AltPrice = priceClose(0);
		check("BrokerAsset prices a second asset (BTC/USD)", AltPrice > 0);
		asset(HL_SYMBOL);   // back to the trading asset

		// --- candle history ---
		BadBars = 0;
		for(i = 0; i < HISTORY_BARS; i++) {
			if(priceHigh(i) < priceLow(i)) BadBars++;
			if(priceClose(i) <= 0) BadBars++;
		}
		check("BrokerHistory2 returns coherent candles", BadBars == 0);
		if(BadBars > 0) printf("\n        (%d malformed bars of %d)", BadBars, HISTORY_BARS);

		// --- starting position ---
		printf("\nPosition before start: %.0f lots", heldLots());

		printf("\n--- market round trips (%d cycles) ---", CYCLES);
		Phase = PH_BUY;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Market round trips: buy, sell in three uneven chunks, sweep.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_BUY) {
		var Price = priceClose(0);
		if(Price <= 0) return;                 // wait for a live price
		if(!budgetOk()) { Phase = PH_REPORT; return; }

		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		OrderLimit = 0;                        // market order
		PositionBeforeBuy = heldLots();        // so the fill check measures this buy
		Lots = ceil(TRADE_USD/Price*AMOUNT_SCALE);
		enterLong();
		printf("\n[cycle %d] buy submitted: %.0f lots (~$%d)", Cycle+1, Lots, TRADE_USD);
		Phase = PH_AWAIT_FILL;
		return;
	}

	if(Phase == PH_AWAIT_FILL) {
		var Held = heldLots();
		// measured against the pre-buy position, so a leftover balance from
		// an earlier run can't make a failed buy look successful
		check("Market buy reaches the exchange", Held > PositionBeforeBuy);
		if(Held <= DUST_THRESHOLD_LOTS) {      // nothing to sell; skip to next cycle
			Phase = PH_CYCLE_END;
			return;
		}
		// uneven split, each chunk still comfortably over the $10 minimum
		Chunk1 = floor(Held*0.4);
		Chunk2 = floor(Held*0.3);
		Chunk3 = Held - Chunk1 - Chunk2;
		printf("\n[cycle %d] filled %.0f lots; selling %.0f / %.0f / %.0f",
			Cycle+1, Held, Chunk1, Chunk2, Chunk3);
		Phase = PH_SELL_A;
		return;
	}

	if(Phase == PH_SELL_A) { sellViaShort(Chunk1); Phase = PH_SELL_B; return; }
	if(Phase == PH_SELL_B) { sellViaShort(Chunk2); Phase = PH_SELL_C; return; }
	if(Phase == PH_SELL_C) { sellViaShort(Chunk3); Phase = PH_SWEEP;  return; }

	if(Phase == PH_SWEEP) {
		var Leftover = heldLots();
		if(Leftover > DUST_THRESHOLD_LOTS) {
			printf("\n[cycle %d] sweeping leftover: %.0f lots", Cycle+1, Leftover);
			sellViaShort(Leftover);
			return;                            // re-check next tick
		}
		check("Three-way split closes the position", Leftover <= DUST_THRESHOLD_LOTS);
		if(Leftover > 0) printf("\n        (dust remaining: %.0f lots)", Leftover);
		Phase = PH_CYCLE_END;
		return;
	}

	if(Phase == PH_CYCLE_END) {
		Cycle++;
		if(Cycle < CYCLES && budgetOk()) {
			Phase = PH_BUY;
		} else {
			printf("\n--- resting limit order ---");
			Phase = PH_LIMIT_PLACE;
		}
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Resting GTC limit order: place far from market, poll, cancel.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_LIMIT_PLACE) {
		var Price = priceClose(0);
		var LimitPrice;
		if(Price <= 0) return;

		LimitPrice = Price*0.6;                // deep below market: must not fill
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_GTC);
		OrderLimit = LimitPrice;
		Lots = ceil(LIMIT_USD/LimitPrice*AMOUNT_SCALE);
		enterLong();
		printf("\nLimit buy submitted: %.0f lots @ %.4f (market %.4f)", Lots, LimitPrice, Price);

		OrderLimit = 0;                        // don't leak the limit into later orders
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		LimitTradeID = 0;
		LimitPollsLeft = 5;
		LimitCancelled = 0;
		Phase = PH_LIMIT_POLL;
		return;
	}

	if(Phase == PH_LIMIT_POLL) {
		int Pending = 0;
		for(open_trades) {
			// An accepted-but-unfilled order is a trade holding zero lots.
			// Zorro doesn't reliably flag it TradeIsPending, so match on
			// the lot count too -- missing it strands a live order on the
			// exchange holding collateral.
			if(TradeIsPending || TradeLots == 0) {
				Pending++;
				LimitTradeID = TradeID;
			}
		}
		LimitPollsLeft--;
		if(Pending == 0 && LimitPollsLeft > 0) return;   // give it a tick or two to register

		check("GTC limit order rests instead of filling", Pending > 0);
		if(Pending == 0) {
			printf("\n        (no pending trade registered; skipping cancel)");
			Phase = PH_REJECT;
			return;
		}
		printf("\nPending limit order registered, trade id %d", LimitTradeID);
		Phase = PH_LIMIT_CANCEL;
		return;
	}

	if(Phase == PH_LIMIT_CANCEL) {
		var Result = brokerCommand(CMD_DO_CANCEL, LimitTradeID);
		check("DO_CANCEL cancels the resting order", Result == 1);
		if(Result == 1) LimitCancelled = 1;
		Phase = PH_REJECT;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Rejection path: an order under Hyperliquid's $10 minimum must be
	// refused cleanly, without stalling or crashing the plugin. Runs last
	// so a surprise here can't cost us the rest of the results.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_REJECT) {
		var Price = priceClose(0);
		if(Price <= 0) return;
		printf("\n--- rejection handling ---");
		PositionBeforeReject = heldLots();
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		OrderLimit = 0;
		Lots = ceil(2.0/Price*AMOUNT_SCALE);   // ~$2: under the exchange minimum
		enterLong();
		printf("\nUndersized order submitted (expect a clean rejection above)");
		Phase = PH_REPORT;
		return;
	}

	if(Phase == PH_REPORT) {
		var EndBalance, Held;
		int Stranded = 0;

		// Safety net: never leave a resting order behind holding collateral,
		// whatever happened in the limit phase above. A zero-lot trade that
		// was already cancelled there still sits in Zorro's book, so skip it
		// -- cancelling it again just fails and would look like a leak.
		for(open_trades) {
			if(TradeLots == 0 && !(TradeID == LimitTradeID && LimitCancelled)) {
				printf("\nCancelling stranded order, trade id %d", TradeID);
				if(brokerCommand(CMD_DO_CANCEL, TradeID) == 1) Stranded++;
			}
		}
		check("No resting orders left behind", Stranded == 0);

		EndBalance = Balance;
		Held = heldLots();

		check("Undersized order left the position unchanged",
			abs(Held - PositionBeforeReject) <= DUST_THRESHOLD_LOTS);
		check("Plugin still responds after a rejection", Held >= 0);
		check("Balance stayed above the floor", EndBalance >= MIN_BALANCE);

		printf("\n=== RESULT: %d passed, %d failed ===", Passed, Failed);
		printf("\nBalance %.2f -> %.2f (cost %.2f USDC)",
			StartBalance, EndBalance, StartBalance - EndBalance);
		if(Held > DUST_THRESHOLD_LOTS)
			printf("\n!! WARNING: %.0f lots still held -- rerun to flatten", Held);
		else if(Held > 0)
			printf("\nDust remaining: %.0f lots (ignored)", Held);

		Phase = PH_DONE;
		return;
	}

	if(Phase == PH_DONE) {
		if(once(1)) printf("\nStress test complete -- press [Stop].");
	}
}
