// Hyperliquid Stress Test (Lite-C, for 32-bit Zorro.exe) ////////////////
// Exercises far more of the plugin than the buy/sell demo: account reads,
// multiple assets, candle history, repeated market round trips with uneven
// splits, a resting GTC limit order with cancellation, and a deliberately
// rejected undersized order. Positions close in several pieces through
// exitLong, so each buy is retired by repeated partial closes. Every
// check logs PASS/FAIL and the run ends with a tally.
//
// Broker: Hyperliquid. User: blank. Password: your private key.
// Demo = testnet. [Trade] mode only.
//
// Budget: holds at most ~$75 of testnet USDC at once. Spread and fees
// cost cents, but each round trip also leaves just under one PURR it can
// never sell (see below), about $5 a cycle. Aborts immediately if the
// account balance would drop under MIN_BALANCE.
//
// Traded on PURR/USDC, the testnet spot pair that trades nearly every
// minute. Hyperliquid has no candle for a minute without trades, so a
// quiet pair leaves holes in Zorro's history: FTEST/USDC ("@171"), finer
// at 0.01-coin steps, went days without a trade. PURR trades only in
// whole coins, one lot each (LotAmount 1). A spot buy pays its fee in the
// coin, so the trade holds a fraction under a whole number of coins; the
// fraction can never be sold and stays in the wallet until Hyperliquid
// sweeps it, so each cycle is measured from the position before its buy.
// HYPE/USDC ("@1035") is unusable: it quotes a ~22% spread and ran from
// $188 to $5480 inside two hours.

#define HL_SYMBOL       "PURR/USDC"
// Priced only, a second asset from the perp universe. BTC/USD ("@50")
// went a day without a trade, so without a bar.
#define ALT_SYMBOL      "ETH-PERP"
#define MIN_BALANCE     750          // hard floor; abort rather than spend past this
// Per cycle, split in three. Every chunk must clear the exchange's $10
// minimum at its slippage-adjusted price, 5% under the bid, so the
// smallest (30%) chunk needs ~$11; 60 leaves it at ~$18.
#define TRADE_USD       60
// Resting limit notional, kept clear of the $10 minimum.
#define LIMIT_USD       15
#define CYCLES          2            // market round trips
#define HISTORY_BARS    50           // candles to pull via BrokerHistory2

// Lots per coin of the current asset, from the asset list's LotAmount:
// 100000 for assets traded in fractions, 1 for those traded only in whole
// coins. The plugin refuses an entry whose LotAmount does not match.
#define AMOUNT_SCALE    (1./LotAmount)
// One exchange size step for HL_SYMBOL: PURR has szDecimals 0, so it
// trades in whole coins, which is 1 lot. Less than this cannot be sold
// at all, so it is the floor a close can reach, not a tolerance.
#define DUST_THRESHOLD_LOTS 1.
// A sweep that stops making progress would otherwise retry forever.
#define MAX_SWEEPS      3
// One size step, the unit chunks are cut in.
#define STEP_LOTS       1.

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
var PositionAtStart;     // coins the account held before the run
var PositionBeforeBuy;
var PositionBeforeReject;
int LimitTradeID;
var LimitAsked;       // limit price the script asked for
var LimitResting;     // price the resting order carries, per Zorro
int LimitPollsLeft;
int LimitCancelled;
int SweepsLeft;
var BeforeSell;      // position before the last partial close, 0 once checked
int SellMisses;      // partial closes this cycle that sold nothing

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

// Costs Zorro has booked across every trade. The plugin reports fees
// through BrokerTrade's pRoll, which lands here; money paid is negative.
var bookedCost()
{
	var Sum = 0;
	for(all_trades) Sum += TradeRoll;
	return Sum;
}

// Real exchange position in lots, independent of Zorro's local trade list.
var heldLots()
{
	return brokerCommand(CMD_GET_POSITION, SymbolTrade);
}

// Closes part of the open long through Zorro's own close path, which
// reaches the plugin as BrokerSell2. Partial, so one buy is closed in
// several pieces.
void sellPart(var SellLots)
{
	BeforeSell = heldLots();
	exitLong(0, 0, SellLots);
}

// Counts the last partial close as a miss if the exchange position did
// not drop. A refused close is otherwise invisible here: Zorro retries it
// quietly, and a later sweep can still flatten the account, so the run
// used to pass with dozens of rejections in its log.
void noteSale()
{
	if(BeforeSell <= 0) return;
	if(heldLots() >= BeforeSell) {
		SellMisses++;
		printf("\n        (that close sold nothing)");
	}
	BeforeSell = 0;
}

// Trades Zorro still holds lots in. The exchange being flat is not enough:
// a trade Zorro thinks is open keeps retrying its close for as long as it
// runs, and quit() tries once more on the way out.
int openTrades()
{
	int N = 0;
	for(open_trades) if(TradeLots > 0) N++;
	return N;
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
	// Crypto trades around the clock, but Zorro defaults to a Monday-to-
	// Friday week and goes "Market closed" from Friday evening UTC until
	// Sunday night -- a run started then sits idle for the whole weekend.
	// BarMode 0 drops BR_WEEKEND, and the week runs Monday 00:00 to Sunday
	// 23:59, so neither the bars nor the trading pause.
	BarMode = 0;
	StartWeek = 10000;
	EndWeek = 72359;
	LookBack = HISTORY_BARS;   // forces a real BrokerHistory2 fetch
	TickTime = 1000;
	set(LOGFILE);
	set(NFA|OFF);
	SaveMode = 0;
	// Default hedging: closes go through Zorro's close path rather than
	// being faked with an opposing entry.
	Hedge = 0;

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
		var MaxGap;
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
		check("BrokerAsset prices the spot pair", Price > 0);
		// The spread comes from the live order book; the asset list's
		// figure is replaced by it, and 0 would mean the feed is not used.
		printf("\nSpread %.4f (%.2f%%)", Spread, 100.*Spread/max(Price, 0.000001));
		check("BrokerAsset reports a live spread", Spread > 0 && Spread < Price/10);

		asset(ALT_SYMBOL);
		AltPrice = priceClose(0);
		check("BrokerAsset prices a second asset (ETH-PERP)", AltPrice > 0);
		asset(HL_SYMBOL);   // back to the trading asset

		// --- candle history ---
		BadBars = 0;
		for(i = 0; i < HISTORY_BARS; i++) {
			if(priceHigh(i) < priceLow(i)) BadBars++;
			if(priceClose(i) <= 0) BadBars++;
		}
		check("BrokerHistory2 returns coherent candles", BadBars == 0);
		if(BadBars > 0) printf("\n        (%d malformed bars of %d)", BadBars, HISTORY_BARS);
		// Bars missing from the history leave Zorro trading on old prices
		// without an error. Hyperliquid has no candle for a minute without
		// trades, and Zorro's bars follow every asset loaded: one that had
		// not traded for 44 minutes broke the lookback there. The plugin
		// fills such minutes with flat bars, so every step is one bar.
		MaxGap = 0;
		for(i = 0; i < HISTORY_BARS-1; i++)
			MaxGap = max(MaxGap, (wdate(i) - wdate(i+1))*1440);
		printf("\nWidest step between bars: %.0f min", MaxGap);
		check("The price history has a bar for every minute", MaxGap < 1.5);

		// --- starting position ---
		PositionAtStart = heldLots();
		printf("\nPosition before start: %.0f lots", PositionAtStart);

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
		SellMisses = 0;
		BeforeSell = 0;
		Lots = ceil(TRADE_USD/Price*AMOUNT_SCALE);
		enterLong();
		printf("\n[cycle %d] buy submitted: %.0f lots (~$%d)", Cycle+1, Lots, TRADE_USD);
		Phase = PH_AWAIT_FILL;
		return;
	}

	if(Phase == PH_AWAIT_FILL) {
		// What this buy added. Measured against the pre-buy position, so a
		// leftover from an earlier cycle or run can't make a failed buy
		// look successful, nor be sold as if it were part of this trade.
		var Held = heldLots() - PositionBeforeBuy;
		check("Market buy reaches the exchange", Held > 0);
		if(Held <= DUST_THRESHOLD_LOTS) {      // nothing to sell; skip to next cycle
			Phase = PH_CYCLE_END;
			return;
		}
		// Uneven split in whole size steps: a fractional chunk is floored by
		// the plugin, and on a coarse pair that can take it under the $10
		// minimum (one run had such a close refused dozens of times).
		// The last chunk takes the remainder, rounded up, so it closes the
		// whole trade and the plugin retires the sliver with it. Zorro cuts
		// a lot count to a whole number: 4.99 lots of PURR went out as 4,
		// a partial close that left the trade open.
		Chunk1 = floor(Held*0.4/STEP_LOTS)*STEP_LOTS;
		Chunk2 = floor(Held*0.3/STEP_LOTS)*STEP_LOTS;
		Chunk3 = ceil(Held - Chunk1 - Chunk2);
		printf("\n[cycle %d] filled %.0f lots; selling %.0f / %.0f / %.0f",
			Cycle+1, Held, Chunk1, Chunk2, Chunk3);
		Phase = PH_SELL_A;
		return;
	}

	if(Phase == PH_SELL_A) { sellPart(Chunk1); Phase = PH_SELL_B; return; }
	if(Phase == PH_SELL_B) { noteSale(); sellPart(Chunk2); Phase = PH_SELL_C; return; }
	if(Phase == PH_SELL_C) { noteSale(); sellPart(Chunk3); SweepsLeft = MAX_SWEEPS; Phase = PH_SWEEP; return; }

	if(Phase == PH_SWEEP) {
		var Leftover;
		noteSale();
		Leftover = heldLots() - PositionBeforeBuy;   // what is left of this cycle's buy
		if(Leftover > DUST_THRESHOLD_LOTS && SweepsLeft > 0) {
			SweepsLeft--;
			printf("\n[cycle %d] sweeping leftover: %.0f lots (%d tries left)",
				Cycle+1, Leftover, SweepsLeft);
			sellPart(Leftover);
			return;                            // re-check next tick
		}
		check("Partial closes retire the whole position", Leftover <= DUST_THRESHOLD_LOTS);
		if(Leftover > DUST_THRESHOLD_LOTS)
			printf("\n        (gave up after %d sweeps)", MAX_SWEEPS);
		else if(Leftover > 0)
			printf("\n        (%.0f lots left, under one size step)", Leftover);
		check("Every partial close sold on the exchange", SellMisses == 0);
		if(SellMisses > 0) printf("\n        (%d closes sold nothing)", SellMisses);
		check("Zorro's trade is closed, not just the exchange position", openTrades() == 0);
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
		LimitAsked = LimitPrice;
		LimitResting = 0;
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
				LimitResting = TradePriceOpen;
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
		// The plugin reports the price the order went out at. Zorro rounds
		// limits to the asset's PIP first, and a PIP of 1 once sent a 2.7477
		// limit out at 3. Five significant figures allow 0.01% of rounding.
		printf("\nLimit asked %.6f, resting at %.6f", LimitAsked, LimitResting);
		check("The limit order rests at the price asked for",
			abs(LimitResting - LimitAsked) <= LimitAsked*0.0001);
		Phase = PH_LIMIT_CANCEL;
		return;
	}

	if(Phase == PH_LIMIT_CANCEL) {
		var Result = brokerCommand(CMD_DO_CANCEL, LimitTradeID);
		check("DO_CANCEL cancels the resting order", Result == 1);
		// DO_CANCEL removes the order on the exchange but not from Zorro's
		// own list, where it lingered as a zero-lot "open" trade on the
		// status page for the rest of the run. cancelTrade drops it from
		// the list without sending anything to the broker.
		if(Result == 1) { LimitCancelled = 1; cancelTrade(LimitTradeID); }
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
		var EndBalance, Held, Gained;
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
		check("Zorro holds no open trades at the end", openTrades() == 0);
		// Coins the account owned before the run are not this run's
		// leftovers. Each close sells whole coins, topping up the fee's
		// sliver from them, so the run should end no richer in coins.
		Gained = Held - PositionAtStart;
		check("The run leaves no more coins than it started with",
			Gained <= DUST_THRESHOLD_LOTS);
		check("Balance stayed above the floor", EndBalance >= MIN_BALANCE);
		// Spot pays a taker fee on every fill. Zero here would mean
		// Zorro's P&L is gross and the cost never reached it.
		printf("\nCost booked by Zorro: %.4f", bookedCost());
		check("Fees reach Zorro as a trade cost", bookedCost() < 0);

		printf("\n=== RESULT: %d passed, %d failed ===", Passed, Failed);
		printf("\nBalance %.2f -> %.2f (cost %.2f USDC)",
			StartBalance, EndBalance, StartBalance - EndBalance);
		if(Gained > DUST_THRESHOLD_LOTS)
			printf("\n!! WARNING: %.2f lots more than at the start -- rerun to flatten", Gained);
		else if(Gained > 0)
			printf("\nDust remaining: %.2f lots -- under one size step, unsellable", Gained);

		Phase = PH_DONE;
		return;
	}

	// quit() force-closes whatever Zorro still has open, unlike [Stop] with
	// AutoConfirm = 1. By here PH_SWEEP has flattened the exchange position
	// and PH_REPORT has cancelled any resting order, so Zorro's list is
	// empty and there is nothing left to close.
	if(Phase == PH_DONE) {
		printf("\nStress test complete -- stopping.");
		quit("Spot stress test complete");
	}
}
