# liquidzorro

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
   +-- assets   -- (Phase 3) Hyperliquid spot asset universe <-> Zorro asset list
   +-- state    -- logged-in clients + Zorro's message/progress callbacks
```

## Source layout

| File | Role |
| --- | --- |
| `Cargo.toml` | Declares the crate as a `cdylib`; depends on `hyperliquid_rust_sdk` (path dependency on the sibling SDK checkout), `alloy` (for `PrivateKeySigner`), and `tokio`. |
| `src/lib.rs` | Crate root; wires up the module tree and re-exports the FFI surface. |
| `src/ffi.rs` | The Zorro-facing C ABI exports. `BrokerOpen`, `BrokerLogin`, `BrokerTime`, `BrokerAsset`, `BrokerHistory2` are implemented against Hyperliquid testnet; `BrokerBuy2`/`BrokerTrade`/`BrokerAccount` land in Phase 3. Signatures verified against a real Zorro S 2026 install's `trading.h` and `Simulation.cpp` sample plugin. |
| `src/bridge.rs` | Sync-to-async bridge: owns the `login`/`asset_price`/`history` calls that block on the Tokio runtime to drive the async SDK, plus the OLE-date <-> Unix-time conversions Zorro's `DATE` type needs. |
| `src/state.rs` | Shared plugin state: the logged-in `InfoClient`/`ExchangeClient`, and the `fpError`/`fpProgress` callback pointers captured in `BrokerOpen` (used to report messages back to Zorro instead of stdout). |
| `src/runtime.rs` | Owns the single lazily-started Tokio runtime used to drive the async SDK from Zorro's blocking calls. |
| `src/assets.rs` | Placeholder for Phase 3: mapping Hyperliquid's spot asset universe to Zorro's expected asset list format. |

## Local setup

Set `HL_TESTNET_PRIVKEY` to a Hyperliquid testnet private key before `BrokerLogin` can succeed; never commit it (see `.gitignore`'s `.env` entry).
