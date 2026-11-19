// No orders: logs in, reads the account and one price, and quits.
//
// zorrorun.py's first check on a Zorro install: that "Zorro -trade" starts
// a run, that it logs in on testnet with the key from the environment, and
// that it ends by itself. Prints the same PASS/FAIL/RESULT lines as the
// stress tests.

int Passed, Failed;

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

function run()
{
	BarPeriod = 1;
	// Crypto trades around the clock; see the stress tests.
	BarMode = 0;
	StartWeek = 10000;
	EndWeek = 72359;
	LookBack = 2;
	set(LOGFILE);
	SaveMode = 0;

	if(!is(TRADEMODE)) {
		printf("\nThe probe only runs in [Trade] mode.");
		quit(0);
		return;
	}
	if(is(INITRUN)) assetList("AssetsHyperliquid");
	asset("ETH-PERP");
	if(is(INITRUN) || is(LOOKBACK)) return;

	printf("\n=== Hyperliquid probe ===");
	check("The account reports a positive value", Balance > 0);
	check("BrokerAsset prices ETH-PERP", priceClose(0) > 0);
	check("Zorro holds no open trades", NumOpenTotal == 0);
	printf("\n=== RESULT: %d passed, %d failed ===", Passed, Failed);
	quit("Probe complete");
}
