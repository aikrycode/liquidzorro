//! Sync-to-async bridge between Zorro's blocking FFI calls and the async
//! hyperliquid_rust_sdk clients.
//!
//! TODO(Phase 2/3): request/response calls (orders, account info) run via
//! `runtime::get().block_on(...)`; streaming market data (WS subscriptions)
//! is pushed into a mutex-protected cache by a background task so polling
//! calls like `BrokerAsset`/`BrokerTime` never block on a network round-trip.
