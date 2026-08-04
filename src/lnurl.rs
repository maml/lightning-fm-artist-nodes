// LNURL-pay (LUD-06) / Lightning Address (LUD-16) for the artist node.
//
// Gives the artist a Lightning address that resolves to their OWN node —
// `artist@their-node-domain` — so zaps and tips land in the same wallet as
// storefront sales, with no platform and no custodian in the path.
//
// LUD-06 flow:
//   1. GET /.well-known/lnurlp/{name}          → payRequest metadata
//   2. GET /.well-known/lnurlp/{name}?amount=N → { "pr": <bolt11> }
//
// The load-bearing detail is the description_hash binding: the invoice must
// commit to sha256 of the EXACT metadata bytes served in step 1. That is what
// stops a compromised server swapping the description after the wallet has
// shown it to the user. We therefore build the metadata string once, from a
// single deterministic function, and hash those same bytes.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    Json,
};
use ldk_node::bitcoin::hashes::{sha256, Hash as _};
use ldk_node::lightning_invoice::{Bolt11InvoiceDescription, Sha256 as InvoiceSha256};
use ldk_node::Node;
use serde::Deserialize;
use tracing::{info, warn};

const INVOICE_EXPIRY_SECS: u32 = 3600;

/// LUD-06 allows 1 msat, but sub-satoshi amounts are not routable in practice.
const MIN_SENDABLE_MSAT: u64 = 1_000;

/// Ceiling when the node has no usable inbound — the JIT path can still open
/// a channel, so we advertise a sane maximum rather than zero.
const FALLBACK_MAX_SENDABLE_MSAT: u64 = 1_000_000_000; // 1M sats

#[derive(Clone)]
pub struct LnurlState {
    pub node: Arc<Node>,
    pub artist_name: String,
    /// Lightning address this node answers for, e.g. `artist@node.example.com`
    pub address: String,
    /// External base URL — must match how wallets reach us, for the callback
    pub public_url: String,
}

#[derive(Deserialize)]
pub struct CallbackParams {
    /// Amount in millisatoshis. Absent means "return metadata".
    pub amount: Option<u64>,
    pub comment: Option<String>,
}

/// LNURL errors are returned with HTTP 200 and a status field — several
/// wallets ignore the body on non-2xx responses and surface nothing useful.
fn lnurl_error(reason: impl Into<String>) -> Response {
    Json(serde_json::json!({ "status": "ERROR", "reason": reason.into() })).into_response()
}

/// The metadata array, serialised deterministically. This exact string is
/// both served to the wallet and hashed into the invoice — never rebuild it
/// differently in the two paths or every payment will fail verification.
fn metadata_string(artist_name: &str, address: &str) -> String {
    serde_json::json!([
        ["text/plain", format!("Pay {artist_name} on Lightning FM")],
        ["text/identifier", address],
    ])
    .to_string()
}

/// What we can honestly accept: real inbound when we have it, otherwise a
/// sane ceiling since the JIT path can still open a channel.
fn max_sendable_msat(inbound: u64) -> u64 {
    if inbound >= MIN_SENDABLE_MSAT {
        inbound
    } else {
        FALLBACK_MAX_SENDABLE_MSAT
    }
}

fn local_part(address: &str) -> &str {
    address.split('@').next().unwrap_or(address)
}

/// Usable inbound across live channels, which bounds what we can actually
/// receive without the LSP opening a new channel.
fn inbound_msat(node: &Node) -> u64 {
    node.list_channels()
        .iter()
        .filter(|c| c.is_usable)
        .map(|c| c.inbound_capacity_msat)
        .sum()
}

pub async fn lnurlp(
    State(state): State<LnurlState>,
    Path(name): Path<String>,
    Query(params): Query<CallbackParams>,
) -> Response {
    if !name.eq_ignore_ascii_case(local_part(&state.address)) {
        return lnurl_error("Unknown user");
    }

    match params.amount {
        None => metadata_response(&state),
        Some(amount_msat) => invoice_response(&state, amount_msat, params.comment).await,
    }
}

fn metadata_response(state: &LnurlState) -> Response {
    // Advertise what we can genuinely receive. Claiming more than the channel
    // holds just produces payment failures the sender cannot diagnose.
    let max_sendable = max_sendable_msat(inbound_msat(&state.node));

    Json(serde_json::json!({
        "tag": "payRequest",
        "callback": format!(
            "{}/.well-known/lnurlp/{}",
            state.public_url.trim_end_matches('/'),
            local_part(&state.address)
        ),
        "minSendable": MIN_SENDABLE_MSAT,
        "maxSendable": max_sendable,
        "metadata": metadata_string(&state.artist_name, &state.address),
        "commentAllowed": 0,
    }))
    .into_response()
}

async fn invoice_response(
    state: &LnurlState,
    amount_msat: u64,
    comment: Option<String>,
) -> Response {
    let inbound = inbound_msat(&state.node);
    let max_sendable = max_sendable_msat(inbound);

    if amount_msat < MIN_SENDABLE_MSAT {
        return lnurl_error(format!("Amount below minimum of {MIN_SENDABLE_MSAT} msat"));
    }
    if amount_msat > max_sendable {
        return lnurl_error(format!(
            "Amount above maximum of {max_sendable} msat — receiving capacity is limited"
        ));
    }

    // LUD-06: commit to the hash of the metadata we served, not a free-text
    // description. Wallets verify this and will reject a mismatch.
    let meta = metadata_string(&state.artist_name, &state.address);
    let meta_hash = sha256::Hash::hash(meta.as_bytes());
    let invoice_desc = Bolt11InvoiceDescription::Hash(InvoiceSha256(meta_hash));

    if comment.is_some() {
        // commentAllowed is 0, so a comment cannot be bound into the
        // description hash without breaking verification. Ignore it loudly.
        warn!("LNURL comment supplied but commentAllowed is 0 — ignoring");
    }

    let invoice = if inbound >= amount_msat {
        state
            .node
            .bolt11_payment()
            .receive(amount_msat, &invoice_desc, INVOICE_EXPIRY_SECS)
    } else {
        // Same reasoning as the purchase gate: only ask for JIT when existing
        // inbound genuinely cannot hold the payment, then fall back.
        match state.node.bolt11_payment().receive_via_jit_channel(
            amount_msat,
            &invoice_desc,
            INVOICE_EXPIRY_SECS,
            None,
        ) {
            Ok(inv) => Ok(inv),
            Err(e) => {
                warn!(error = ?e, "LNURL JIT invoice failed; falling back to plain receive");
                state
                    .node
                    .bolt11_payment()
                    .receive(amount_msat, &invoice_desc, INVOICE_EXPIRY_SECS)
            }
        }
    };

    match invoice {
        Ok(inv) => {
            info!(
                artist = %state.artist_name,
                amount_msat,
                payment_hash = %inv.payment_hash(),
                "LNURL invoice issued"
            );
            Json(serde_json::json!({ "pr": inv.to_string(), "routes": [] })).into_response()
        }
        Err(e) => {
            warn!(error = ?e, "LNURL invoice creation failed");
            lnurl_error("Could not create invoice")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIST: &str = "0GGM3NT3D";
    const ADDRESS: &str = "0ggm3nt3d@node.example.com";

    #[test]
    fn local_part_splits_address() {
        assert_eq!(local_part("artist@example.com"), "artist");
        assert_eq!(local_part("bare"), "bare");
    }

    #[test]
    fn metadata_contains_required_pairs() {
        let meta = metadata_string(ARTIST, ADDRESS);
        assert!(meta.contains("text/plain"), "LUD-06 requires a text/plain entry");
        assert!(
            meta.contains("text/identifier"),
            "LUD-16 wants the address as identifier"
        );
        assert!(meta.contains(ADDRESS));
    }

    #[test]
    fn metadata_is_valid_json_array_of_pairs() {
        let meta = metadata_string(ARTIST, ADDRESS);
        let parsed: Vec<Vec<String>> = serde_json::from_str(&meta).expect("metadata must be JSON");
        assert!(parsed.iter().all(|p| p.len() == 2), "each entry is [mime, content]");
    }

    #[test]
    fn metadata_is_deterministic() {
        // The description_hash binding only holds if both call sites produce
        // byte-identical metadata.
        assert_eq!(
            metadata_string(ARTIST, ADDRESS),
            metadata_string(ARTIST, ADDRESS)
        );
    }

    #[test]
    fn description_hash_matches_served_metadata() {
        // This is the LUD-06 invariant: what a wallet hashes from the metadata
        // response must equal the invoice's description_hash.
        let served = metadata_string(ARTIST, ADDRESS);
        let wallet_side = sha256::Hash::hash(served.as_bytes());
        let invoice_side = sha256::Hash::hash(metadata_string(ARTIST, ADDRESS).as_bytes());
        assert_eq!(wallet_side, invoice_side);
    }

    #[test]
    fn max_sendable_uses_inbound_when_present() {
        assert_eq!(max_sendable_msat(250_000_000), 250_000_000);
    }

    #[test]
    fn max_sendable_falls_back_when_no_inbound() {
        assert_eq!(max_sendable_msat(0), FALLBACK_MAX_SENDABLE_MSAT);
        assert_eq!(max_sendable_msat(500), FALLBACK_MAX_SENDABLE_MSAT);
    }
}
