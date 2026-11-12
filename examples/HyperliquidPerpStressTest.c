// Hyperliquid Perpetual Stress Test (Lite-C, for 32-bit Zorro.exe) //////
// Exercises the perpetual path: leverage, a long and a real short, signed
// positions, margin reporting, a resting limit order, and a rejected
// undersized order. Every check logs PASS/FAIL and the run ends with a
// tally. Broker: Hyperliquid. User: blank. Password: your private key.
// Demo = testnet. [Trade] mode only.
//
// Unlike the spot test this uses Zorro's ordinary close path --
// exitLong/exitShort rather than an opposing entry. Spot has no shorting,
// so closing there had to be faked with enterShort and Hedge = 2. A perp
// has a real signed position, so the plugin marks a close with
// StopDist = -1 and sends it reduce-only. Whether that works end to end is
// the main thing this script measures.
//
// Budget: 1x leverage throughout and roughly $15 of exposure at a time.
// Leverage is what separates this from the spot test -- a leveraged
// position can be liquidated, so MAX_LEVERAGE is held at 1 and the run
// aborts below MIN_BALANCE.

#define PERP_SYMBOL     "ETH-PERP"   // Hyperliquid symbol "ETH"
// Priced only, to prove mixed universes. Not HYPE/USDC: its testnet book
// has quoted 756 / 2267, a spread no live-book check should accept.
#define SPOT_SYMBOL     "FTEST/USDC"
#define MIN_BALANCE     750          // hard floor; abort rather than spend past this
#define TRADE_USD       15           // notional per position; must clear $10
#define LIMIT_USD       15           // resting limit order notional
#define MAX_LEVERAGE    1            // unleveraged: no liquidation risk
#define HISTORY_BARS    50

#define AMOUNT_SCALE    100000.
// Dust has to be a cash amount, not a lot count: 1000 lots is $1.58 of HYPE
// but $23 of ETH, which would swallow an entire test position.
#define DUST_USD        1.0
// Stop distance as a share of price. Wide enough that the test cannot
// trigger it, so what is measured is that the exchange accepted and then
// released it -- not whether it fires.
#define STOP_PCT        0.10

// BrokerCommand codes (trading.h)
#define CMD_SET_SYMBOL     132
#define CMD_SET_LEVERAGE   140
#define CMD_GET_POSITION   53
#define CMD_SET_ORDERTYPE  157
#define CMD_DO_CANCEL      301
// Plugin-specific: move a resting order in place. Takes three vars:
// trade id, new limit price, new lots (0 keeps the size).
#define CMD_DO_MODIFY      2001
#define ORDERTYPE_GTC      2
#define ORDERTYPE_IOC      1
// Plugin-specific: GTC that may only add liquidity (Hyperliquid "Alo").
#define ORDERTYPE_POST_ONLY 4
// Plugin-specific: enter as a TWAP over this many minutes; 0 = off.
#define CMD_SET_TWAP       2002
// The exchange's smallest TWAP is $100. Only the first slices fill before
// the test closes it, so it costs cents, not the whole $110.
#define TWAP_USD        110

#define PH_PREFLIGHT    0
#define PH_LEVERAGE     1
#define PH_OPEN_LONG    2
#define PH_CHECK_LONG   3
#define PH_CLOSE_LONG   4
#define PH_CHECK_FLAT1  5
#define PH_OPEN_SHORT   6
#define PH_CHECK_SHORT  7
#define PH_CLOSE_SHORT  8
#define PH_CHECK_FLAT2  9
#define PH_STOP_OPEN    10
#define PH_STOP_CHECK   11
#define PH_STOP_CLOSE   12
#define PH_LIMIT_PLACE  13
#define PH_LIMIT_POLL   14
#define PH_LIMIT_CANCEL 15
#define PH_REJECT       16
#define PH_FLATTEN      17
#define PH_REPORT       18
#define PH_STOP_FLAT    20
#define PH_DONE         19
#define PH_LIMIT_MODIFY 21
#define PH_POST_ONLY    22
#define PH_TWAP_START   23
#define PH_TWAP_POLL    24
#define PH_TWAP_CLOSE   25
#define PH_TWAP_FLAT    26
#define PH_TWAP_STILL   27

int Phase;
int Passed, Failed;
var StartBalance;
var MarginFlat;
var PositionBeforeReject;
int LimitTradeID;
int LimitPollsLeft;
int TwapPollsLeft;
int LimitCancelled;
int FlattenTriesLeft;

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

// Costs Zorro has booked across every trade so far. The plugin reports
// fees and funding through BrokerTrade's pRoll, which lands here, and
// money paid is negative.
var bookedCost()
{
	var Sum = 0;
	for(all_trades) Sum += TradeRoll;
	return Sum;
}

// Signed: negative while short. Reads the exchange, not Zorro's book.
var heldLots()
{
	return brokerCommand(CMD_GET_POSITION, SymbolTrade);
}

// Lots worth less than DUST_USD at the current price, i.e. "close enough
// to flat". Scales with the asset instead of assuming a price.
var dustLots()
{
	var Price = priceClose(0);
	if(Price <= 0) return 0;
	return DUST_USD/Price*AMOUNT_SCALE;
}

// Parameter is not named Lots: that is a predefined Zorro variable and
// using it here expands the macro into the declaration.
int isFlat(var PositionLots)
{
	return abs(PositionLots) <= dustLots();
}

var lotsFor(var Usd)
{
	var Price = priceClose(0);
	if(Price <= 0) return 0;
	return ceil(Usd/Price*AMOUNT_SCALE);
}

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
	LookBack = HISTORY_BARS;
	TickTime = 1000;
	set(LOGFILE);
	set(NFA|OFF);
	SaveMode = 0;
	// Default hedging on purpose: a perp close should go through Zorro's
	// normal close path, not be faked with an opposing entry.
	Hedge = 0;

	if(!is(TRADEMODE)) {
		printf("\nUnsupported function: this test only runs in [Trade] mode.");
		quit(0);
		return;
	}

	if(is(INITRUN)) {
		assetList("AssetsHyperliquid");
		asset(SPOT_SYMBOL);      // register both so their price series load
		Phase = PH_PREFLIGHT;
		Passed = 0;
		Failed = 0;
	}
	asset(PERP_SYMBOL);
	if(is(LOOKBACK)) return;

	///////////////////////////////////////////////////////////////////////
	// Read-only checks, all in one tick.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_PREFLIGHT) {
		int i, BadBars;
		var Price, SpotPrice, Starting;

		printf("\n=== Hyperliquid perpetual stress test ===");

		StartBalance = Balance;
		printf("\nAccount value: %.2f  margin used: %.2f", StartBalance, MarginVal);
		check("BrokerAccount reports a positive account value", StartBalance > 0);
		if(!budgetOk()) { Phase = PH_REPORT; return; }
		check("Balance covers the test budget", StartBalance >= MIN_BALANCE + TRADE_USD + LIMIT_USD);
		MarginFlat = MarginVal;   // baseline, so the long measures its own margin

		Price = priceClose(0);
		check("BrokerAsset prices the perpetual", Price > 0);

		// Funding arrives as Zorro's rollover, per lot per day. The asset
		// list says 0, so anything else came from the plugin; a long pays
		// what a short receives, so the two mirror each other.
		printf("\nRollLong %.3e  RollShort %.3e per lot/day", RollLong, RollShort);
		check("BrokerAsset reports funding as rollover", RollLong != 0 && RollLong == -RollShort);
		printf("\nSpread %.2f (%.3f%%)", Spread, 100.*Spread/Price);
		check("Perp spread comes from the live book", Spread > 0 && Spread < Price/10);

		asset(SPOT_SYMBOL);
		SpotPrice = priceClose(0);
		check("BrokerAsset still prices a spot pair", SpotPrice > 0);
		check("A spot pair reports no rollover", RollLong == 0 && RollShort == 0);
		// Both books stream live, so both spreads are real: positive, and
		// far below the price (a broken book would read as a huge one).
		check("Spot spread comes from the live book", Spread > 0 && Spread < SpotPrice/10);
		asset(PERP_SYMBOL);
		printf("\n%s @ %.2f, %s @ %.4f", PERP_SYMBOL, Price, SPOT_SYMBOL, SpotPrice);

		BadBars = 0;
		for(i = 0; i < HISTORY_BARS; i++) {
			if(priceHigh(i) < priceLow(i)) BadBars++;
			if(priceClose(i) <= 0) BadBars++;
		}
		check("BrokerHistory2 returns coherent perpetual candles", BadBars == 0);

		Starting = heldLots();
		printf("\nPosition before start: %.0f lots (dust threshold %.0f)", Starting, dustLots());
		check("Account starts flat", isFlat(Starting));
		if(!isFlat(Starting)) {
			printf("\n!! An open position exists -- close it before running.");
			Phase = PH_REPORT;
			return;
		}

		Phase = PH_LEVERAGE;
		return;
	}

	if(Phase == PH_LEVERAGE) {
		var Result;
		printf("\n--- leverage ---");
		brokerCommand(CMD_SET_SYMBOL, SymbolTrade);
		Result = brokerCommand(CMD_SET_LEVERAGE, MAX_LEVERAGE);
		check("SET_LEVERAGE is accepted", Result == MAX_LEVERAGE);
		Phase = PH_OPEN_LONG;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Long: open, verify positive, close through exitLong.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_OPEN_LONG) {
		if(!budgetOk()) { Phase = PH_REPORT; return; }
		printf("\n--- long ---");
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		OrderLimit = 0;
		Lots = lotsFor(TRADE_USD);
		if(Lots <= 0) return;             // wait for a price
		enterLong();
		printf("\nLong submitted: %.0f lots (~$%d)", Lots, TRADE_USD);
		Phase = PH_CHECK_LONG;
		return;
	}

	if(Phase == PH_CHECK_LONG) {
		var Held = heldLots();
		var Notional = Held/AMOUNT_SCALE*priceClose(0);
		var Expected = Notional/MAX_LEVERAGE;
		printf("\nPosition while long: %.0f lots (~$%.2f notional)", Held, Notional);
		check("A long reports a positive position", Held > dustLots());
		// At 1x the exchange locks up the whole notional. A wrong leverage
		// reading shows up here as margin off by that factor.
		printf("\nMargin used: %.2f (expected ~%.2f at %dx)",
			MarginVal - MarginFlat, Expected, MAX_LEVERAGE);
		check("Margin used matches the notional at this leverage",
			abs(MarginVal - MarginFlat - Expected) < Expected*0.25);
		// Opening a position spends only a fee. On a unified account the
		// margin also shows as a spot "hold", and counting both once made
		// the balance jump by the whole margin.
		printf("\nBalance while long: %.2f (started %.2f)", Balance, StartBalance);
		check("An open position does not inflate the balance", abs(Balance - StartBalance) < 1);
		Phase = PH_CLOSE_LONG;
		return;
	}

	if(Phase == PH_CLOSE_LONG) {
		// Zorro's own close. It reaches the plugin as BrokerBuy2 with a
		// negative amount and StopDist = -1, which goes out reduce-only.
		exitLong();
		printf("\nexitLong submitted");
		Phase = PH_CHECK_FLAT1;
		return;
	}

	if(Phase == PH_CHECK_FLAT1) {
		var Held = heldLots();
		var Cost = bookedCost();
		printf("\nPosition after exitLong: %.0f lots", Held);
		check("exitLong closes the long", isFlat(Held));
		// The round trip paid exchange fees. If this reads zero, Zorro's
		// P&L is gross and the plugin is not reporting what it cost.
		printf("\nCost booked by Zorro: %.4f", Cost);
		check("Fees reach Zorro as a trade cost", Cost < 0);
		Phase = PH_OPEN_SHORT;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Short: the case spot cannot express at all.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_OPEN_SHORT) {
		if(!budgetOk()) { Phase = PH_REPORT; return; }
		printf("\n--- short ---");
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		OrderLimit = 0;
		Lots = lotsFor(TRADE_USD);
		if(Lots <= 0) return;
		enterShort();
		printf("\nShort submitted: %.0f lots (~$%d)", Lots, TRADE_USD);
		Phase = PH_CHECK_SHORT;
		return;
	}

	if(Phase == PH_CHECK_SHORT) {
		var Held = heldLots();
		printf("\nPosition while short: %.0f lots", Held);
		check("A short reports a NEGATIVE position", Held < -dustLots());
		Phase = PH_CLOSE_SHORT;
		return;
	}

	if(Phase == PH_CLOSE_SHORT) {
		exitShort();
		printf("\nexitShort submitted");
		Phase = PH_CHECK_FLAT2;
		return;
	}

	if(Phase == PH_CHECK_FLAT2) {
		var Held = heldLots();
		printf("\nPosition after exitShort: %.0f lots", Held);
		check("exitShort closes the short", isFlat(Held));
		Phase = PH_STOP_OPEN;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Stop loss held by the exchange. StopFactor = 1 hands the stop to the
	// broker instead of Zorro watching the price itself, which is what
	// keeps a position protected while Zorro is not running.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_STOP_OPEN) {
		var Price = priceClose(0);
		if(Price <= 0) return;
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		OrderLimit = 0;
		StopFactor = 1;                   // the exchange owns this stop
		Stop = STOP_PCT*Price;            // distance, not a level
		Lots = lotsFor(TRADE_USD);
		if(Lots <= 0) return;
		enterLong();
		printf("\nLong with a stop %.2f below %.2f", Stop, Price);
		Phase = PH_STOP_CHECK;
		return;
	}

	if(Phase == PH_STOP_CHECK) {
		var Held = heldLots();
		printf("\nPosition with a stop attached: %.0f lots", Held);
		check("A stopped entry still opens the position", Held > dustLots());
		// A stop this far away must not cost the position. Firing here
		// would mean the trigger went in on the wrong side.
		check("A stop far from the market does not fire", !isFlat(Held));
		Phase = PH_STOP_CLOSE;
		return;
	}

	if(Phase == PH_STOP_CLOSE) {
		exitLong();
		Stop = 0;
		StopFactor = 0;                   // back to Zorro-side stops
		printf("\nexitLong submitted; the stop should go with it");
		Phase = PH_STOP_FLAT;
		return;
	}

	if(Phase == PH_STOP_FLAT) {
		var Held = heldLots();
		printf("\nPosition after closing the stopped trade: %.0f lots", Held);
		check("Closing a stopped trade still flattens it", isFlat(Held));
		// A stop left resting would act on whatever position came next,
		// so the sweep at the end checks nothing is left behind.
		Phase = PH_LIMIT_PLACE;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Resting limit order. Perp prices allow fewer decimals than spot, so
	// this also checks the perpetual rounding rule.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_LIMIT_PLACE) {
		var Price = priceClose(0);
		var LimitPrice;
		if(Price <= 0) return;
		printf("\n--- resting limit order ---");

		LimitPrice = Price*0.6;           // far below market: must not fill
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_GTC);
		OrderLimit = LimitPrice;
		Lots = ceil(LIMIT_USD/LimitPrice*AMOUNT_SCALE);
		enterLong();
		printf("\nLimit buy submitted: %.0f lots @ %.4f (market %.4f)", Lots, LimitPrice, Price);

		OrderLimit = 0;
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
			// An accepted-but-unfilled order holds zero lots; Zorro does not
			// always flag it TradeIsPending.
			if(TradeIsPending || TradeLots == 0) {
				Pending++;
				LimitTradeID = TradeID;
			}
		}
		LimitPollsLeft--;
		if(Pending == 0 && LimitPollsLeft > 0) return;

		check("A perpetual GTC limit order rests instead of filling", Pending > 0);
		if(Pending == 0) {
			printf("\n        (no pending trade registered; skipping cancel)");
			Phase = PH_REJECT;
			return;
		}
		printf("\nPending limit order registered, trade id %d", LimitTradeID);
		Phase = PH_LIMIT_MODIFY;
		return;
	}

	// Moves the resting order further from the market and makes it bigger
	// in one request, the way a strategy chases or backs off a quote.
	// Still far below market, so it cannot fill.
	if(Phase == PH_LIMIT_MODIFY) {
		var Price = priceClose(0);
		var Args[3];
		var Result;
		int StillPending = 0;
		if(Price <= 0) return;
		Args[0] = LimitTradeID;
		Args[1] = Price*0.55;
		Args[2] = ceil(1.5*LIMIT_USD/(Price*0.55)*AMOUNT_SCALE);
		Result = brokerCommand(CMD_DO_MODIFY, Args);
		printf("\nDO_MODIFY %d -> %.0f lots @ %.2f: %.0f", LimitTradeID, Args[2], Args[1], Result);
		check("DO_MODIFY moves the resting order in place", Result == 1);
		for(open_trades) if(TradeID == LimitTradeID) StillPending = 1;
		check("The modified order is still Zorro's pending trade", StillPending);
		Phase = PH_LIMIT_CANCEL;
		return;
	}

	if(Phase == PH_LIMIT_CANCEL) {
		var Result = brokerCommand(CMD_DO_CANCEL, LimitTradeID);
		check("DO_CANCEL cancels the resting perpetual order", Result == 1);
		// DO_CANCEL removes the order on the exchange but not from Zorro's
		// own list, where it lingered as a zero-lot "open" trade on the
		// status page for the rest of the run. cancelTrade drops it from
		// the list without sending anything to the broker.
		if(Result == 1) { LimitCancelled = 1; cancelTrade(LimitTradeID); }
		Phase = PH_POST_ONLY;
		return;
	}

	// A post-only buy priced through the ask must be refused, not filled
	// as a taker: that is the whole point of post-only.
	if(Phase == PH_POST_ONLY) {
		var Price = priceClose(0);
		var Before = heldLots();
		TRADE* Entered;
		if(Price <= 0) return;
		printf("\n--- post-only ---");
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_POST_ONLY);
		OrderLimit = Price*1.02;
		Lots = ceil(LIMIT_USD/Price*AMOUNT_SCALE);
		Entered = enterLong();
		OrderLimit = 0;
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		check("A post-only order through the book is refused", Entered == 0);
		check("The refused post-only order left the position unchanged", isFlat(heldLots() - Before));
		Phase = PH_TWAP_START;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// TWAP: the exchange slices one order into small ones every ~30s. To
	// Zorro it is a trade that starts empty and grows; closing it must stop
	// the slicing, or the next slice reopens what was just closed.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_TWAP_START) {
		var Price = priceClose(0);
		var Result;
		if(Price <= 0) return;
		printf("
--- TWAP ---");
		Result = brokerCommand(CMD_SET_TWAP, 4);
		check("SET_TWAP refuses a length the exchange would refuse", Result == -1);
		Result = brokerCommand(CMD_SET_TWAP, 5);
		check("SET_TWAP accepts 5 minutes", Result == 5);
		Lots = ceil(TWAP_USD/Price*AMOUNT_SCALE);
		enterLong();
		// Off at once, so no later entry goes out as a TWAP by accident.
		brokerCommand(CMD_SET_TWAP, 0);
		printf("
TWAP buy submitted: %.0f lots (~$%d) over 5 minutes", Lots, TWAP_USD);
		TwapPollsLeft = 3;
		Phase = PH_TWAP_POLL;
		return;
	}

	if(Phase == PH_TWAP_POLL) {
		var ZorroLots = 0, Held = heldLots();
		int Trades = 0;
		for(open_trades) { Trades++; ZorroLots += TradeLots; }
		printf("
TWAP so far: Zorro %.0f lots in %d trade(s), exchange %.0f lots", ZorroLots, Trades, Held);
		if(ZorroLots <= 0 && --TwapPollsLeft > 0) return;
		check("A TWAP entry is one Zorro trade", Trades == 1);
		check("The TWAP trade grows as its slices fill", ZorroLots > 0);
		// A slice can land between Zorro's poll and this one, so the
		// exchange may hold one slice more, never less.
		check("Zorro's TWAP trade matches what the exchange holds",
			ZorroLots > 0 && Held >= ZorroLots - dustLots());
		Phase = PH_TWAP_CLOSE;
		return;
	}

	if(Phase == PH_TWAP_CLOSE) {
		exitLong();
		printf("
exitLong submitted; the TWAP should stop with it");
		Phase = PH_TWAP_FLAT;
		return;
	}

	if(Phase == PH_TWAP_FLAT) {
		var Held = heldLots();
		printf("
Position after closing the TWAP trade: %.0f lots", Held);
		check("Closing a TWAP trade flattens everything it bought", isFlat(Held));
		Phase = PH_TWAP_STILL;
		return;
	}

	// A minute on: two more slices would have landed if it still ran.
	if(Phase == PH_TWAP_STILL) {
		var Held = heldLots();
		int Trades = 0;
		for(open_trades) if(TradeLots > 0) Trades++;
		printf("
Position a minute later: %.0f lots", Held);
		check("The closed TWAP buys nothing more", isFlat(Held));
		check("Zorro holds no TWAP trade afterwards", Trades == 0);
		Phase = PH_REJECT;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// An order under the exchange minimum must be refused cleanly. Last,
	// so a surprise here cannot cost the rest of the results.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_REJECT) {
		var Price = priceClose(0);
		if(Price <= 0) return;
		printf("\n--- rejection handling ---");
		PositionBeforeReject = heldLots();
		brokerCommand(CMD_SET_ORDERTYPE, ORDERTYPE_IOC);
		OrderLimit = 0;
		Lots = ceil(2.0/Price*AMOUNT_SCALE);   // ~$2: under the minimum
		enterLong();
		printf("\nUndersized order submitted (expect a clean rejection above)");
		FlattenTriesLeft = 3;
		Phase = PH_FLATTEN;
		return;
	}

	///////////////////////////////////////////////////////////////////////
	// Leave the account flat before stopping: cancel anything resting and
	// close any exposure still open, retrying a few times since an IOC
	// close can fill partially. Doing it here rather than leaving it to
	// [Stop] means the exchange is verifiably flat while the script can
	// still report on it.
	///////////////////////////////////////////////////////////////////////
	if(Phase == PH_FLATTEN) {
		var Held = heldLots();
		int Acted = 0;

		// A cancelled order still sits in Zorro's book holding zero lots;
		// skip that one, it is already gone on the exchange.
		for(open_trades) {
			if(TradeLots == 0 && !(TradeID == LimitTradeID && LimitCancelled)) {
				printf("\nCancelling resting order, trade id %d", TradeID);
				if(brokerCommand(CMD_DO_CANCEL, TradeID) == 1) Acted++;
			}
		}

		if(!isFlat(Held)) {
			printf("\nClosing residual position: %.0f lots", Held);
			if(Held > 0) exitLong();
			else exitShort();
			Acted++;
		}

		FlattenTriesLeft--;
		if(Acted > 0 && FlattenTriesLeft > 0) return;   // settle, then re-check

		Phase = PH_REPORT;
		return;
	}

	if(Phase == PH_REPORT) {
		var EndBalance, Held;
		int Resting = 0;

		// PH_FLATTEN has already cancelled and closed; anything still here
		// resisted it.
		for(open_trades) {
			if(TradeLots == 0 && !(TradeID == LimitTradeID && LimitCancelled)) Resting++;
		}
		check("No resting orders left behind", Resting == 0);

		EndBalance = Balance;
		Held = heldLots();

		check("Undersized order left the position unchanged",
			abs(Held - PositionBeforeReject) <= dustLots());
		check("Account ends flat", isFlat(Held));
		// The exchange being flat is not enough: a trade Zorro still thinks
		// is open retries its close for as long as it runs.
		Resting = 0;
		for(open_trades) if(TradeLots > 0) Resting++;
		check("Zorro holds no open trades at the end", Resting == 0);
		check("Balance stayed above the floor", EndBalance >= MIN_BALANCE);

		printf("\n=== RESULT: %d passed, %d failed ===", Passed, Failed);
		printf("\nAccount value %.2f -> %.2f (cost %.2f USDC)",
			StartBalance, EndBalance, StartBalance - EndBalance);
		printf("\nMargin used at end: %.2f", MarginVal);
		if(!isFlat(Held))
			printf("\n!! WARNING: %.0f lots still open -- close them manually", Held);

		Phase = PH_DONE;
		return;
	}

	// Safe to quit here: quit() force-closes open trades, and PH_FLATTEN
	// has already cancelled every resting order and closed every position,
	// with PH_REPORT confirming the exchange is flat and Zorro's own list
	// holding no open trades.
	if(Phase == PH_DONE) {
		printf("\nPerpetual stress test complete -- stopping.");
		quit("Perpetual stress test complete");
	}
}
