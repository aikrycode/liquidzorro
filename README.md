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
Zorro.exe
   |  synchronous C ABI calls: BrokerOpen / BrokerLogin / BrokerAsset /
   |  BrokerBuy2 / BrokerTrade / BrokerSell2 / BrokerAccount / BrokerTime / ...
   v
liquidzorro.dll  (Rust cdylib)
   +-- ffi      -- extern "C" exports matching trading.h exactly
   +-- runtime  -- single Tokio runtime, owned for the DLL's lifetime
   +-- bridge   -- sync -> async: blocking calls (orders, account info) use
   |               runtime.block_on(...); streaming data (price/order
   |               updates) is cached by a background task so polling calls
   |               never block on a network round-trip
   +-- hyperliquid-rust-sdk (path dependency) -- InfoClient + ExchangeClient
   +-- assets   -- Hyperliquid spot asset universe <-> Zorro asset list
   +-- state    -- shared clients + live cache + trade-id mapping
```

## Source layout (Phase 1 scaffolding)

| File | Role |
| --- | --- |
| `Cargo.toml` | Declares the crate as a `cdylib` and pulls in `hyperliquid_rust_sdk` (path dependency on the sibling SDK checkout) and `tokio`. |
| `src/lib.rs` | Crate root; wires up the module tree and re-exports the FFI surface. |
| `src/ffi.rs` | The Zorro-facing C ABI exports (`BrokerOpen`, `BrokerLogin`, ...). Currently stub implementations reconstructed from the Zorro manual; signatures must be re-verified against the real `trading.h`/`variables.h` before Phase 2. |
| `src/runtime.rs` | Owns the single lazily-started Tokio runtime used to drive the async SDK from Zorro's blocking calls. |
| `src/state.rs` | Shared plugin state: Hyperliquid clients, asset map, and trade-id tracking (populated starting Phase 2). |
| `src/bridge.rs` | Placeholder for the sync<->async bridging logic between Zorro's blocking FFI calls and the SDK's async calls. |
| `src/assets.rs` | Placeholder for mapping Hyperliquid's spot asset universe to Zorro's expected asset list format. |
