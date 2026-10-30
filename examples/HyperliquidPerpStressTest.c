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
#define SPOT_SYMBOL     "HYPE/USDC"  // priced only, to prove mixed universes
#define MIN_BALANCE     750          // hard floor; abort rather than spend past this
#define TRADE_USD       15           // notional per position; must clear $10
#define LIMIT_USD       15           // resting limit order notional
#define MAX_LEVERAGE    1            // unleveraged: no liquidation risk
#define HISTORY_BARS    50

#define AMOUNT_SCALE    100000.
// Dust has to be a cash amount, not a lot count: 1000 lots is $1.58 of HYPE
// but $23 of ETH, which would swallow an entire test position.
#define DUST_USD        1.0

// BrokerCommand codes (trading.h)
#define CMD_SET_SYMBOL     132
#define CMD_SET_LEVERAGE   140
#define CMD_GET_POSITION   53
#define CMD_SET_ORDERTYPE  157
#define CMD_DO_CANCEL      301
#define ORDERTYPE_GTC      2
#define ORDERTYPE_IOC      1

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
#define PH_LIMIT_PLACE  10
#define PH_LIMIT_POLL   11
#define PH_LIMIT_CANCEL 12
#define PH_REJECT       13
#define PH_FLATTEN      14
#define PH_REPORT       15
#define PH_DONE         16

int Phase;
int Passed, Failed;
var StartBalance;
var PositionBeforeReject;
int LimitTradeID;
int LimitPollsLeft;
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
		check("Margin used is reported and not negative", MarginVal >= 0);

		Price = priceClose(0);
		check("BrokerAsset prices the perpetual", Price > 0);

		asset(SPOT_SYMBOL);
		SpotPrice = priceClose(0);
		check("BrokerAsset still prices a spot pair", SpotPrice > 0);
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
		printf("\nPosition while long: %.0f lots", Held);
		check("A long reports a positive position", Held > dustLots());
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
		printf("\nPosition after exitLong: %.0f lots", Held);
		check("exitLong closes the long", isFlat(Held));
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
		Phase = PH_LIMIT_CANCEL;
		return;
	}

	if(Phase == PH_LIMIT_CANCEL) {
		var Result = brokerCommand(CMD_DO_CANCEL, LimitTradeID);
		check("DO_CANCEL cancels the resting perpetual order", Result == 1);
		if(Result == 1) LimitCancelled = 1;
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
	// with PH_REPORT confirming the exchange is flat. The only entry left
	// in Zorro's book is the cancelled limit order, holding zero lots.
	if(Phase == PH_DONE) {
		printf("\nPerpetual stress test complete -- stopping.");
		quit("Perpetual stress test complete");
	}
}
