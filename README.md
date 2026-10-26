# liquidzorro

> **Disclaimer.** This software is pre-release, experimental ("beta") software provided for testing and development purposes. It interacts with live financial exchanges and can place real orders and move real funds. It is provided "AS IS", WITHOUT WARRANTY OF ANY KIND, express or implied, including but not limited to warranties of merchantability, fitness for a particular purpose, accuracy, or non-infringement. The authors and contributors assume no liability and disclaim all responsibility for any financial loss, data loss, trading loss, missed trades, erroneous orders, or other damages of any kind arising from the use, misuse, or inability to use this software, whether on testnet or mainnet. By using this code, you acknowledge and accept that you do so entirely at your own risk, and that you are solely responsible for reviewing, testing, and validating its behavior before relying on it with any funds.

Zorro broker plugin (C/C++ DLL) for trading Hyperliquid perpetuals and spot.

The plugin is written in Rust and compiled as a `cdylib` exposing the plain C
ABI that [Zorro](https://zorro-project.com/manual/toc.php) expects from a
broker plugin (`BrokerOpen`, `BrokerLogin`, `BrokerAsset`, ...). It drives
Hyperliquid's REST/WebSocket API via the official
[hyperliquid-rust-sdk](https://github.com/hyperliquid-dex/hyperliquid-rust-sdk),
bridging Zorro's synchronous, single-threaded calls to that SDK's async
(Tokio-based) API. Initial scope is Hyperliquid **spot** trading; perpetuals
(leverage, margin, funding) follow in a later phase.

## Architecture

```
Zorro64.exe
   |  synchronous C ABI calls: BrokerOpen / BrokerLogin / BrokerTime /
   |  BrokerAsset / BrokerHistory2 / BrokerCommand / BrokerBuy2 / BrokerTrade / ...
   |  (there is no BrokerSell2 -- a negative Amount to BrokerBuy2 sells/closes)
   v
liquidzorro.dll  (Rust cdylib, in Zorro's Plugin64\ folder)
   +-- ffi      -- extern "C" exports, verified against a real trading.h/Simulation.cpp
   +-- runtime  -- single Tokio runtime, owned for the DLL's lifetime
   +-- bridge   -- sync -> async: each FFI call does a short runtime.block_on(...)
   |               over a REST request today; streaming (WS) caching is a later
   |               optimization once polling performance needs it
   +-- hyperliquid-rust-sdk (path dependency) -- InfoClient + ExchangeClient
   +-- assets   -- (later) Hyperliquid spot asset universe <-> Zorro asset list
   +-- state    -- logged-in clients + open-trade bookkeeping + Zorro's callbacks
```

## Source layout

| File | Role |
| --- | --- |
| `Cargo.toml` | Declares the crate as a `cdylib`; depends on `hyperliquid_rust_sdk` (path dependency on the sibling SDK checkout), `alloy` (for `PrivateKeySigner`), and `tokio`. |
| `src/lib.rs` | Crate root; wires up the module tree and re-exports the FFI surface. |
| `src/ffi.rs` | The Zorro-facing C ABI exports: `BrokerOpen`, `BrokerLogin`, `BrokerTime`, `BrokerAsset`, `BrokerHistory2`, `BrokerAccount`, `BrokerBuy2`, `BrokerTrade`, and a stub `BrokerCommand`. Signatures verified against a real Zorro S 2026 install's `trading.h` and `Simulation.cpp` sample plugin. |
| `src/bridge/mod.rs` | Sync-to-async bridge: `login`/`asset_price`/`history`/`place_order`/`trade_status`/`account_balance`, each blocking on the Tokio runtime to drive the async SDK, plus the OLE-date <-> Unix-time conversions Zorro's `DATE` type needs. |
| `src/bridge/tests.rs` | `#[ignore]`d live smoke tests against Hyperliquid testnet, named by what they verify (see Local setup). |
| `src/state.rs` | Shared plugin state: the logged-in `InfoClient`/`ExchangeClient`/wallet address, open-trade bookkeeping (Zorro trade id -> Hyperliquid `oid`), and the `fpError`/`fpProgress` callback pointers captured in `BrokerOpen` (used to report messages back to Zorro instead of stdout). |
| `src/runtime.rs` | Owns the single lazily-started Tokio runtime used to drive the async SDK from Zorro's blocking calls. |
| `src/assets.rs` | Placeholder for a later pass: mapping Hyperliquid's spot asset universe to Zorro's expected asset list format (tick size, real spread) instead of the `BrokerAsset` placeholders currently in `ffi.rs`. |
| `tests/ffi_harness.rs` | Loads the compiled `liquidzorro.dll` dynamically and calls every export through its real C signature with real raw pointers and callback functions -- the only test that exercises the actual FFI boundary the way Zorro will, rather than calling our Rust functions directly. Network-free (runs with `cargo test`, no `--ignored` needed). |

Every exported function in `ffi.rs` is also wrapped in a panic guard: since an unwinding Rust panic crossing an `extern "C"` boundary is undefined behavior, any unexpected panic is caught and turned into an ordinary failure return (logged via the error callback) instead of risking a crash of Zorro's whole process.

## Local setup

### Hyperliquid account setup, step by step

This is the exact sequence that worked when setting this project up. It walks through creating a dedicated wallet, getting it recognized by Hyperliquid, and getting testnet funds into it -- the awkward part being that Hyperliquid's testnet faucet will not release funds to a wallet with no mainnet history, so a small real-money step is unavoidable.

1. **Generate a dedicated wallet/private key for this project.** Don't reuse a wallet you use elsewhere. A throwaway EVM key can be generated locally with `alloy`'s `PrivateKeySigner::random()` (see git history for the one-off helper script used here), or with any wallet tool you trust (e.g. create a brand-new account in MetaMask). You need the private key (`0x`-prefixed hex) and the address it corresponds to.
2. **Important mindset shift once you fund it:** the moment you send any real money to this wallet (step 3), it stops being a disposable throwaway secret and becomes a wallet holding real value. Treat the private key accordingly -- don't paste it somewhere it'll be logged or shared if you can avoid it, and if it ever is exposed, treat any funds in it as at risk.
3. **Send a small amount of real USDC to that wallet on Arbitrum One.** A few dollars is enough (this project used ~6 USDC). This is required before Hyperliquid's testnet faucet will cooperate -- see step 5.
4. **Deposit that USDC into Hyperliquid mainnet.** Go to `app.hyperliquid.xyz`, connect the wallet, and deposit the USDC from Arbitrum into Hyperliquid itself (minimum to deposit seems to be 5 USDC). This is real money on a real exchange -- small amount, but real.
5. **Go to the separate testnet app and connect the same wallet.** `app.hyperliquid-testnet.xyz` is a completely separate environment from mainnet -- connecting a wallet there doesn't touch mainnet funds or state. Hyperliquid's testnet faucet is anti-sybil gated: it checks for a real mainnet deposit history (step 4) before it will let a wallet claim test funds, which is why step 3-4 had to happen first.
6. **Claim the testnet faucet.** Once connected on the testnet app, find the "Faucet" option (under the account/portfolio menu, or on the root page if it's not visible from `/trade`) and claim it. This credits ~1000 testnet USDC -- play money with no real value, usable for as much order-placing/cancelling as you like without financial consequence.
7. **Keep the private key handy.** Inside Zorro, it goes in the Password field (see "Installing and running inside Zorro" below); for the Rust-level dev/test workflow below, set it as an environment variable instead (never commit it -- see `.gitignore`'s `.env` entry):
   ```powershell
   $env:HL_TESTNET_PRIVKEY = "0x..."
   ```

### Running the live smoke tests

With `HL_TESTNET_PRIVKEY` set (step 7 above), the `#[ignore]`d tests in `src/bridge/tests.rs` exercise the real testnet API:

```powershell
cargo test --lib -- --ignored --nocapture
```

`connects_and_reads_market_data` is read-only (spot_meta/all_mids/candles_snapshot) and never spends anything. `logout_then_login_reconnects` checks that logging out actually drops the session and the next login reconnects -- also read-only. `places_and_closes_a_spot_order` places and immediately closes an order sized just above Hyperliquid's $10 minimum notional, sweeping any partial-fill leftover afterward -- it does spend a little testnet USDC, but only slippage is lost (a few cents), since the round trip returns the rest. `flattens_leftover_balance` is a standalone cleanup if a prior run ever leaves a position open. All need a funded testnet wallet per the steps above.

## Installing and running inside Zorro

> Verified against a live Zorro 3.1.1.2 session on the 32-bit `Zorro.exe` + Lite-C path: plugin load, login/logout/re-login, price and history reads, and a full buy / sell / sell round trip on Hyperliquid testnet. For the 64-bit path see [Known Issues](#known-issues).

1. **Build the release DLL**:
   ```powershell
   cargo build --release
   ```
   This produces `target\release\liquidzorro.dll`.
2. **Copy it into Zorro's plugin folder**, naming it after the broker (matching the convention every other broker plugin DLL in that folder follows, e.g. `Oanda.dll`, `Bittrex.dll`):
   ```powershell
   Copy-Item target\release\liquidzorro.dll "<ZorroFolder>\Plugin64\Hyperliquid.dll"
   ```
   Zorro64 loads plugins from `Plugin64\`; 32-bit `Zorro.exe` uses `Plugin\` and needs its own 32-bit build (`cargo build --release --target i686-pc-windows-msvc`).
3. **Launch Zorro.** It scans the plugin folder and lists what it finds -- ours appears as `Hyperliquid` in the **Broker** dropdown.
4. **Select "Hyperliquid".** User: blank. Password: your private key (`0x...`); copy-paste it into the Password field as-is, mind its exact length. Demo/Real toggle selects testnet/mainnet directly via `BrokerLogin` -- start with Demo.
5. **Trigger login** (enter Trade or Test mode) and watch the message window -- that's where `BrokerLogin` reports success or failure.
6. **Copy the asset list.** Put `AssetsHyperliquid.csv` and `Accounts.csv` (both in `examples\`) into `History\`. They point the Hyperliquid accounts at the right asset list and login settings.
7. **In `Zorro.ini`, set `AutoConfirm = 1`,** then restart Zorro. This stops Zorro prompting to close trades when you press `[Stop]`; the strategies close their own positions, so the prompt is never needed.

You can switch between Demo and Real, or paste a different key, and press `[Trade]` again without restarting Zorro -- the plugin reconnects on each login.

### Demo strategy walkthrough

`examples/HyperliquidBuySellDemo.c` (32-bit Lite-C) / `HyperliquidBuySellDemo+.cpp` (64-bit, needs external compilation -- see its header) is a linear state machine, advancing one step per `run()` call (~once a minute, since `BarPeriod = 1`):

- **Setup:**
  - `set(NFA|OFF)` -- lifts Zorro's NFA trading restrictions.
  - `SaveMode = 0` -- starts each run from a clean slate.
  - `Hedge = 2` -- lets a sell stand on its own rather than being treated as a position reversal.
  - `!is(TRADEMODE)` -- restricts the script to `[Trade]`.
- **`Step == -1`:** reads the live Hyperliquid position via `GET_POSITION`. A real position resumes directly into selling; anything under `DUST_THRESHOLD_LOTS` is reported and left alone.
- **`Step == 0`:** waits for a live price, sizes `Lots` from `USD_AMOUNT`, `enterLong()`.
- **`Step == 1`:** waits for the fill, splits the total into `Lots1`/`Lots2`.
- **`Step == 2`/`3`:** sells each half through `sellViaShort()`, a helper around `enterShort()`. Hyperliquid spot has no shorting, so the plugin treats it as a plain sell. The plugin's own `BrokerBuy2` OK/FAILED line records what actually happened.
- **`Step == 4`:** re-reads `GET_POSITION` and sweeps anything above the dust threshold, covering partially filled sells.
- **`AMOUNT_SCALE` (100000):** `Lots` counts `1/AMOUNT_SCALE` sub-units, letting a whole-number `Amount` express fractional coin quantities. Matches `bridge::AMOUNT_SCALE` and the CSV's `LotAmount` column.

### Stress test strategy

`examples/HyperliquidStressTest.c` exercises much more of the plugin than the demo. Select it in Zorro and press `[Trade]`; it runs unattended for roughly 20 minutes, logging `PASS`/`FAIL` per check and a tally at the end.

What it covers:

- Account balance, and pricing two different assets.
- 50 bars of candle history, checked for coherence.
- Two market round trips, each selling the position back in three uneven parts.
- A resting limit order placed below market, then cancelled.
- An order under the exchange's $10 minimum, to confirm it is refused cleanly.

It holds at most ~$57 at once and loses only spread. Edit `MIN_BALANCE` (default 750) to set the floor it refuses to trade below, and `CYCLES` to change how many round trips it makes.

## Known Issues

- **64-bit Zorro:** the plugin loads and "Hyperliquid" appears in the Broker dropdown, but strategies don't start -- cause not yet established. Use the 32-bit path, which is verified end to end.
