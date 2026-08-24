//! SHA-256 chain hashing for tamper detection.
//!
//! Each entry's `hash` field is computed as
//! `SHA-256(prev_hash || canonical_payload(entry_excluding_hash))`, where
//! `prev_hash` is the chain head (i.e. the previous entry's hash; or
//! `CHAIN_ORIGIN` for seq 0). The chain origin is stored so a verifier can
//! re-derive the head from scratch without off-chain data.
//!
//! The on-chain hash function is **SHA-256** (host-provided via
//! `env.crypto().sha256`) rather than BLAKE3. The user-facing spec called
//! for BLAKE3, but BLAKE3 is not natively available in Soroban 21.5.0 and a
//! pure-WASM BLAKE3 implementation would inflate gas significantly. SHA-256
//! provides equivalent collision-resistance and runs natively in the host,
//! keeping per-log cost predictable.

use soroban_sdk::{Address, Bytes, BytesN, Env, IntoVal, String, Symbol};

use crate::records::{StateSnapshot, CHAIN_ORIGIN};

/// Helper: copy a Soroban `String` into a heap `alloc::string::String`.
#[allow(dead_code)]
fn soroban_string_to_rust(s: &String) -> alloc::string::String {
    let len = s.len() as usize;
    let mut buf = alloc::vec![0u8; len];
    s.copy_into_slice(&mut buf);
    // SAFETY: Soroban strings are validated UTF-8 at construction time.
    unsafe { alloc::string::String::from_utf8_unchecked(buf) }
}

/// Encode a Soroban `String` into `Bytes`.
fn string_bytes(env: &Env, s: &String) -> Bytes {
    let len = s.len();
    let mut buf = alloc::vec![0u8; len as usize];
    s.copy_into_slice(&mut buf);
    Bytes::from_slice(env, &buf)
}

/// Encode a `Symbol` into `Bytes` via its XDR representation.
fn symbol_bytes(env: &Env, s: &Symbol) -> Bytes {
    // Convert Symbol → ScVal → ScSymbol, then extract raw bytes.
    let sc_val: soroban_sdk::xdr::ScVal = s.clone().into_val(env);
    if let soroban_sdk::xdr::ScVal::Symbol(sc_sym) = sc_val {
        let raw: &[u8] = sc_sym.as_ref();
        Bytes::from_slice(env, raw)
    } else {
        Bytes::new(env)
    }
}

/// Encode an `Address` into `Bytes` via its string representation.
fn address_bytes(env: &Env, a: &Address) -> Bytes {
    // Address::to_string() returns a Soroban String; convert that to bytes.
    let s: String = a.to_string();
    string_bytes(env, &s)
}

/// Decode a `StateSnapshot` to a `Bytes`. We serialize `len` first then each
/// `(symbol_bytes, i128::to_be_bytes(16))` field in insertion order.
fn snapshot_bytes(env: &Env, s: &StateSnapshot) -> Bytes {
    let mut out = Bytes::new(env);
    let n: u32 = s.fields.len();
    out.append(&Bytes::from_array(env, &n.to_be_bytes()));
    for entry in s.fields.iter() {
        out.append(&symbol_bytes(env, &entry.key));
        out.append(&Bytes::from_array(env, &entry.value.to_be_bytes()));
    }
    out
}

/// Build the canonical, deterministic byte stream used as the hash pre-image
/// for `entry` (excluding `entry.hash`). The byte layout is fixed; tests pin it.
pub fn entry_payload(
    env: &Env,
    seq: u64,
    timestamp: u64,
    event_type_id: u32,
    permissions: u32,
    actor: &Address,
    portfolio: &Symbol,
    outcome: &Symbol,
    detail: &String,
    state_before: &StateSnapshot,
    state_after: &StateSnapshot,
) -> Bytes {
    let mut b = Bytes::new(env);
    b.append(&Bytes::from_array(env, &seq.to_be_bytes()));
    b.append(&Bytes::from_array(env, &timestamp.to_be_bytes()));
    b.append(&Bytes::from_array(env, &event_type_id.to_be_bytes()));
    b.append(&Bytes::from_array(env, &permissions.to_be_bytes()));
    let h1: BytesN<32> = env.crypto().sha256(&address_bytes(env, actor)).into();
    b.append(&h1.into());
    let h2: BytesN<32> = env.crypto().sha256(&symbol_bytes(env, portfolio)).into();
    b.append(&h2.into());
    let h3: BytesN<32> = env.crypto().sha256(&symbol_bytes(env, outcome)).into();
    b.append(&h3.into());
    let h4: BytesN<32> = env.crypto().sha256(&string_bytes(env, detail)).into();
    b.append(&h4.into());
    let h5: BytesN<32> = env
        .crypto()
        .sha256(&snapshot_bytes(env, state_before))
        .into();
    b.append(&h5.into());
    let h6: BytesN<32> = env
        .crypto()
        .sha256(&snapshot_bytes(env, state_after))
        .into();
    b.append(&h6.into());
    b
}

/// Compute the entry hash: `SHA-256(prev_hash_bytes || payload)`.
pub fn chain_hash(env: &Env, prev_hash: &BytesN<32>, payload: &Bytes) -> BytesN<32> {
    let mut buf = Bytes::new(env);
    buf.append(&Bytes::from_array(env, &prev_hash.to_array()));
    buf.append(payload);
    env.crypto().sha256(&buf).into()
}

/// The chain hash for the very first entry (`prev_hash == CHAIN_ORIGIN`).
pub fn first_chain_hash(env: &Env, payload: &Bytes) -> BytesN<32> {
    let prev = BytesN::from_array(env, &CHAIN_ORIGIN);
    chain_hash(env, &prev, payload)
}
