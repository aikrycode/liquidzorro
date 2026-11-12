//! Signs and sends exchange actions the SDK has no method for (TWAP).
//!
//! Hyperliquid signs every trading action the same way: hash the action
//! as MessagePack with the nonce, then sign that hash as an EIP-712
//! "Agent". The SDK does exactly this but keeps it private, so it is
//! repeated here, checked against the SDK's own published signatures.
//! A wrong signature cannot misplace an order: the exchange recovers a
//! different signer and refuses the action.

use std::sync::atomic::{AtomicU64, Ordering};

use alloy::{
    dyn_abi::Eip712Domain,
    primitives::{keccak256, Address, B256},
    signers::{local::PrivateKeySigner, Signature, SignerSync},
    sol,
    sol_types::{eip712_domain, SolStruct},
};
use serde::Serialize;

sol! {
    struct Agent {
        string source;
        bytes32 connectionId;
    }
}

/// The hash an action is signed under: MessagePack of the action, the
/// nonce big-endian, then a vault flag.
pub fn action_hash<T: Serialize>(action: &T, nonce: u64, vault: Option<Address>) -> Result<B256, String> {
    let mut bytes = rmp_serde::to_vec_named(action).map_err(|e| format!("encode action: {e}"))?;
    bytes.extend(nonce.to_be_bytes());
    match vault {
        Some(vault) => {
            bytes.push(1);
            bytes.extend(vault);
        }
        None => bytes.push(0),
    }
    Ok(keccak256(bytes))
}

/// Signs an action hash. `source` "a" is mainnet, "b" testnet, so a
/// testnet signature is worthless on mainnet and the reverse.
pub fn sign(wallet: &PrivateKeySigner, connection_id: B256, is_mainnet: bool) -> Result<Signature, String> {
    let agent = Agent { source: if is_mainnet { "a" } else { "b" }.to_string(), connectionId: connection_id };
    let domain: Eip712Domain = eip712_domain! {
        name: "Exchange",
        version: "1",
        chain_id: 1337,
        verifying_contract: Address::ZERO,
    };
    wallet
        .sign_hash_sync(&agent.eip712_signing_hash(&domain))
        .map_err(|e| format!("sign action: {e}"))
}

/// A nonce for an action signed here: the time in milliseconds, never
/// repeated. The SDK keeps its own counter, which starts at its load time
/// and steps by one, so it trails the clock and the two do not meet.
pub fn next_nonce() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// The JSON body for `/exchange`, signature as `{r, s, v}` with v 27/28.
pub fn payload<T: Serialize>(action: &T, signature: &Signature, nonce: u64, vault: Option<Address>) -> Result<String, String> {
    let body = serde_json::json!({
        "action": action,
        "signature": {
            "r": signature.r(),
            "s": signature.s(),
            "v": 27 + signature.v() as u64,
        },
        "nonce": nonce,
        "vaultAddress": vault,
    });
    serde_json::to_string(&body).map_err(|e| format!("encode payload: {e}"))
}

/// A size as the exchange's wire format wants it: fixed decimals, then no
/// trailing zeros, so 0.0100 goes out as "0.01" and 3.0 as "3". The hash
/// covers this string, so it must match what the server re-encodes.
pub fn size_to_wire(size: f64, decimals: u32) -> String {
    let fixed = format!("{size:.*}", decimals as usize);
    let trimmed = if fixed.contains('.') { fixed.trim_end_matches('0').trim_end_matches('.') } else { &fixed };
    if trimmed == "-0" { "0".to_string() } else { trimmed.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// The SDK's published test key and signatures
    /// (`signature/create_signature.rs`), so this copy must agree with it.
    #[test]
    fn signs_like_the_sdk() {
        let wallet: PrivateKeySigner =
            "e908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e".parse().unwrap();
        let id = B256::from_str("0xde6c4037798a4434ca03cd05f00e3b803126221375cd1e7eaaaf041768be06eb").unwrap();
        assert_eq!(
            sign(&wallet, id, true).unwrap().to_string(),
            "0xfa8a41f6a3fa728206df80801a83bcbfbab08649cd34d9c0bfba7c7b2f99340f53a00226604567b98a1492803190d65a201d6805e5831b7044f17fd530aec7841c"
        );
        assert_eq!(
            sign(&wallet, id, false).unwrap().to_string(),
            "0x1713c0fc661b792a50e8ffdd59b637b1ed172d9a3aa4d801d9d88646710fb74b33959f4d075a7ccbec9f2374a6da21ffa4448d58d0413a0d335775f680a881431c"
        );
    }

    /// An action written here encodes exactly like the SDK's own struct
    /// for it, tag first: the hash covers those bytes.
    #[test]
    fn hashes_like_the_sdk() {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "camelCase")]
        enum Mine {
            ScheduleCancel { time: u64 },
        }
        let sdk = hyperliquid_rust_sdk::ScheduleCancel { time: Some(1_700_000_000_000) };
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "camelCase")]
        enum Theirs {
            ScheduleCancel(hyperliquid_rust_sdk::ScheduleCancel),
        }
        let nonce = 1_700_000_000_123;
        assert_eq!(
            action_hash(&Mine::ScheduleCancel { time: 1_700_000_000_000 }, nonce, None).unwrap(),
            action_hash(&Theirs::ScheduleCancel(sdk), nonce, None).unwrap(),
        );
    }

    #[test]
    fn nonces_never_repeat() {
        let a = next_nonce();
        let b = next_nonce();
        assert!(b > a);
    }

    #[test]
    fn sizes_go_out_without_trailing_zeros() {
        assert_eq!(size_to_wire(0.0100, 4), "0.01");
        assert_eq!(size_to_wire(3.0, 2), "3");
        assert_eq!(size_to_wire(12.6, 0), "13");
        assert_eq!(size_to_wire(0.00119, 4), "0.0012");
        assert_eq!(size_to_wire(-0.0, 2), "0");
    }
}
