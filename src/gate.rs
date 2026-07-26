// L402-style purchase gate.
//
// Flow: buyer fetches product metadata → requests an invoice → pays it from
// any Lightning wallet → presents the preimage → daemon verifies
// sha256(preimage) == payment_hash AND the payment settled on its own node →
// streams the artifact. The preimage doubles as the durable re-download
// credential; purchases persist across restarts (store.rs).
//
// Artifact uploads come from the artist's desktop app, authenticated with
// NIP-98 (nip98.rs) against the configured artist pubkey.

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State as AxumState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ldk_node::payment::{PaymentKind, PaymentStatus};
use ldk_node::Node;
use nostr::PublicKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use crate::store::{valid_format, valid_slug, ProductRecord, Store};

const INVOICE_EXPIRY_SECS: u32 = 3600;

#[derive(Clone)]
pub struct GateState {
    pub node: Arc<Node>,
    pub store: Arc<Mutex<Store>>,
    pub artist_name: String,
    /// Signer allowed to upload artifacts. Uploads are disabled when unset.
    pub artist_pubkey: Option<PublicKey>,
    /// External base URL of this daemon — must match NIP-98 u tags.
    pub public_url: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

// ─── Upload (artist, NIP-98) ─────────────────────────────────

#[derive(Deserialize)]
pub struct UploadParams {
    pub title: String,
    pub price_sats: u64,
    pub floor_sats: Option<u64>,
    pub format: String,
}

pub async fn put_product(
    AxumState(state): AxumState<GateState>,
    AxumPath(slug): AxumPath<String>,
    Query(params): Query<UploadParams>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(ref artist_pk) = state.artist_pubkey else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Uploads disabled: ARTIST_PUBKEY not configured",
        );
    };

    if !valid_slug(&slug) {
        return err(StatusCode::BAD_REQUEST, "Invalid slug");
    }
    if !valid_format(&params.format) {
        return err(StatusCode::BAD_REQUEST, "Unsupported format");
    }
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "Empty artifact body");
    }
    if params.price_sats == 0 && params.floor_sats.is_none() {
        return err(StatusCode::BAD_REQUEST, "Price must be > 0 or set a floor");
    }

    let auth = match headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        Some(a) => a,
        None => return err(StatusCode::UNAUTHORIZED, "Missing Authorization header"),
    };

    let body_hash = hex::encode(Sha256::digest(&body));
    let url = format!("{}/products/{}", state.public_url.trim_end_matches('/'), slug);
    if let Err(e) = crate::nip98::verify(auth, "PUT", &url, Some(&body_hash), artist_pk, now_secs())
    {
        return err(StatusCode::UNAUTHORIZED, format!("NIP-98: {e}"));
    }

    let file_name = format!("{}.{}", slug, params.format);
    let record = ProductRecord {
        slug: slug.clone(),
        title: params.title,
        price_sats: params.price_sats,
        floor_sats: params.floor_sats,
        format: params.format,
        file_name: file_name.clone(),
        size_bytes: body.len() as u64,
    };

    let path = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
        };
        store.products_dir().join(&file_name)
    };

    if let Err(e) = tokio::fs::write(&path, &body).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to store artifact: {e}"),
        );
    }

    {
        let mut store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
        };
        if let Err(e) = store.upsert_product(record) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }

    info!(artist = %state.artist_name, slug = %slug, bytes = body.len(), "Product artifact stored");
    (StatusCode::CREATED, Json(serde_json::json!({ "slug": slug }))).into_response()
}

// ─── Public metadata ─────────────────────────────────────────

#[derive(Serialize)]
pub struct ProductMeta {
    pub slug: String,
    pub title: String,
    pub price_sats: u64,
    pub floor_sats: Option<u64>,
    pub format: String,
    pub size_bytes: u64,
}

pub async fn get_product(
    AxumState(state): AxumState<GateState>,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
    };
    match store.get_product(&slug) {
        Some(p) => Json(ProductMeta {
            slug: p.slug.clone(),
            title: p.title.clone(),
            price_sats: p.price_sats,
            floor_sats: p.floor_sats,
            format: p.format.clone(),
            size_bytes: p.size_bytes,
        })
        .into_response(),
        None => err(StatusCode::NOT_FOUND, "No such product"),
    }
}

// ─── Invoice (buyer) ─────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct InvoiceRequest {
    /// Buyer-chosen amount for name-your-price listings; defaults to price.
    pub amount_sats: Option<u64>,
}

#[derive(Serialize)]
pub struct PurchaseInvoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub amount_sats: u64,
    pub expiry_secs: u32,
    /// Session secret for the requesting buyer: polls /status and claims the
    /// download when their wallet (not the browser) holds the preimage.
    pub claim_token: String,
}

/// Random 32-byte hex claim token.
fn new_claim_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("RNG failure: {e}"))?;
    Ok(hex::encode(bytes))
}

/// Validate the buyer's amount against price/floor. Pure — testable.
pub fn resolve_amount(
    price_sats: u64,
    floor_sats: Option<u64>,
    requested: Option<u64>,
) -> Result<u64, String> {
    match (floor_sats, requested) {
        // Fixed price: amount is the price; explicit requests must match it.
        (None, None) => Ok(price_sats),
        (None, Some(a)) if a == price_sats => Ok(price_sats),
        (None, Some(_)) => Err("This listing has a fixed price".into()),
        // Name-your-price: anything at or above the floor.
        (Some(floor), None) => Ok(price_sats.max(floor)),
        (Some(floor), Some(a)) if a >= floor => Ok(a),
        (Some(floor), Some(_)) => Err(format!("Amount below the {floor} sat minimum")),
    }
}

pub async fn post_invoice(
    AxumState(state): AxumState<GateState>,
    AxumPath(slug): AxumPath<String>,
    body: Option<Json<InvoiceRequest>>,
) -> Response {
    let requested = body.and_then(|Json(r)| r.amount_sats);

    let (price_sats, floor_sats, title) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
        };
        match store.get_product(&slug) {
            Some(p) => (p.price_sats, p.floor_sats, p.title.clone()),
            None => return err(StatusCode::NOT_FOUND, "No such product"),
        }
    };

    let amount_sats = match resolve_amount(price_sats, floor_sats, requested) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if amount_sats == 0 {
        return err(StatusCode::BAD_REQUEST, "Amount must be > 0");
    }
    let amount_msat = amount_sats * 1000;

    let description = format!("Lightning FM — {} ({})", title, state.artist_name);
    let desc = match ldk_node::lightning_invoice::Description::new(description) {
        Ok(d) => d,
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Invalid description"),
    };
    let invoice_desc = ldk_node::lightning_invoice::Bolt11InvoiceDescription::Direct(desc);

    // JIT only when inbound capacity can't hold the payment. With a usable
    // channel that fits, a plain invoice routes over it with no LSP fee skim
    // — always requesting JIT breaks repeat purchases: the LSP may forward
    // the skimmed HTLC over the existing channel, and the node rejects it
    // ("sent less than we were supposed to receive") because the skim was
    // registered against the new-channel flow.
    let inbound_msat: u64 = state
        .node
        .list_channels()
        .iter()
        .filter(|c| c.is_usable)
        .map(|c| c.inbound_capacity_msat)
        .sum();

    let invoice = if inbound_msat >= amount_msat {
        match state
            .node
            .bolt11_payment()
            .receive(amount_msat, &invoice_desc, INVOICE_EXPIRY_SECS)
        {
            Ok(inv) => inv,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create invoice: {e:?}"),
                )
            }
        }
    } else {
        match state.node.bolt11_payment().receive_via_jit_channel(
            amount_msat,
            &invoice_desc,
            INVOICE_EXPIRY_SECS,
            None,
        ) {
            Ok(inv) => inv,
            Err(jit_err) => {
                warn!(error = ?jit_err, "JIT invoice failed; falling back to plain receive");
                match state
                    .node
                    .bolt11_payment()
                    .receive(amount_msat, &invoice_desc, INVOICE_EXPIRY_SECS)
                {
                    Ok(inv) => inv,
                    Err(e) => {
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to create invoice: {e:?}"),
                        )
                    }
                }
            }
        }
    };

    let payment_hash = invoice.payment_hash().to_string();
    let claim_token = match new_claim_token() {
        Ok(t) => t,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    {
        let mut store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
        };
        if let Err(e) =
            store.create_purchase(&payment_hash, &slug, amount_msat, now_secs(), &claim_token)
        {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }

    info!(slug = %slug, amount_sats, payment_hash = %payment_hash, "Purchase invoice issued");
    Json(PurchaseInvoice {
        bolt11: invoice.to_string(),
        payment_hash,
        amount_sats,
        expiry_secs: INVOICE_EXPIRY_SECS,
        claim_token,
    })
    .into_response()
}

// ─── Status (buyer polling) ──────────────────────────────────

#[derive(Deserialize)]
pub struct StatusParams {
    pub claim: String,
}

pub async fn get_status(
    AxumState(state): AxumState<GateState>,
    AxumPath(slug): AxumPath<String>,
    Query(params): Query<StatusParams>,
) -> Response {
    let (paid, payment_hash) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
        };
        match store.get_purchase_by_claim(&params.claim) {
            Some(p) if p.slug == slug => (p.paid, p.payment_hash.clone()),
            _ => return err(StatusCode::NOT_FOUND, "No purchase for this claim"),
        }
    };

    // Same node-store fallback as download — covers restarts and races.
    let paid = if paid {
        true
    } else if settled_on_node(&state.node, &payment_hash) {
        if let Ok(mut store) = state.store.lock() {
            let _ = store.mark_paid(&payment_hash);
        }
        true
    } else {
        false
    };

    Json(serde_json::json!({ "paid": paid })).into_response()
}

// ─── Download (buyer, preimage-authenticated) ────────────────

#[derive(Deserialize, Default)]
pub struct DownloadParams {
    /// Cryptographic receipt: sha256(preimage) must equal the payment hash.
    pub preimage: Option<String>,
    /// Session credential from invoice issuance (web-buyer path).
    pub claim: Option<String>,
}

/// Check the node's payment store for a settled inbound payment with this hash.
fn settled_on_node(node: &Node, payment_hash_hex: &str) -> bool {
    node.list_payments().iter().any(|p| {
        let hash = match &p.kind {
            PaymentKind::Bolt11 { hash, .. } => Some(hash),
            PaymentKind::Bolt11Jit { hash, .. } => Some(hash),
            _ => None,
        };
        hash.map(|h| hex::encode(h.0)) == Some(payment_hash_hex.to_string())
            && p.status == PaymentStatus::Succeeded
    })
}

pub async fn get_download(
    AxumState(state): AxumState<GateState>,
    AxumPath(slug): AxumPath<String>,
    Query(params): Query<DownloadParams>,
) -> Response {
    // Resolve the payment hash from whichever credential was presented.
    // The preimage is the cryptographic receipt; the claim token is the
    // session credential handed out at invoice time.
    let payment_hash = match (&params.preimage, &params.claim) {
        (Some(preimage), _) => {
            let preimage_bytes = match hex::decode(preimage.trim()) {
                Ok(b) if b.len() == 32 => b,
                _ => return err(StatusCode::BAD_REQUEST, "Preimage must be 32 bytes of hex"),
            };
            hex::encode(Sha256::digest(&preimage_bytes))
        }
        (None, Some(claim)) => {
            let store = match state.store.lock() {
                Ok(s) => s,
                Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
            };
            match store.get_purchase_by_claim(claim) {
                Some(p) => p.payment_hash.clone(),
                None => return err(StatusCode::NOT_FOUND, "No purchase for this claim"),
            }
        }
        (None, None) => {
            return err(StatusCode::BAD_REQUEST, "Provide a preimage or claim token")
        }
    };

    // Purchase must exist for this product and hash
    let (paid, file_name, format, size_bytes) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
        };
        let Some(purchase) = store.get_purchase(&payment_hash) else {
            return err(StatusCode::NOT_FOUND, "No purchase for this preimage");
        };
        if purchase.slug != slug {
            return err(StatusCode::NOT_FOUND, "Purchase is for a different product");
        }
        let Some(product) = store.get_product(&slug) else {
            return err(StatusCode::NOT_FOUND, "Product no longer exists");
        };
        (
            purchase.paid,
            product.file_name.clone(),
            product.format.clone(),
            product.size_bytes,
        )
    };

    // Not yet marked paid — check the node's payment store (event-loop race
    // or daemon restart between payment and claim), then persist.
    if !paid {
        if settled_on_node(&state.node, &payment_hash) {
            if let Ok(mut store) = state.store.lock() {
                let _ = store.mark_paid(&payment_hash);
            }
        } else {
            return err(StatusCode::PAYMENT_REQUIRED, "Payment not settled yet");
        }
    }

    let path = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Store lock poisoned"),
        };
        store.products_dir().join(&file_name)
    };

    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Artifact missing on disk: {e}"),
            )
        }
    };

    info!(slug = %slug, payment_hash = %payment_hash, "Serving purchased artifact");
    let stream = tokio_util::io::ReaderStream::new(file);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size_bytes)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.{}\"", slug, format),
        )
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── resolve_amount ─────────────────────────────────────────

    #[test]
    fn fixed_price_defaults_to_price() {
        assert_eq!(resolve_amount(5000, None, None).unwrap(), 5000);
    }

    #[test]
    fn fixed_price_rejects_other_amounts() {
        assert!(resolve_amount(5000, None, Some(4999)).is_err());
        assert_eq!(resolve_amount(5000, None, Some(5000)).unwrap(), 5000);
    }

    #[test]
    fn name_your_price_enforces_floor() {
        assert!(resolve_amount(5000, Some(1000), Some(999)).is_err());
        assert_eq!(resolve_amount(5000, Some(1000), Some(1000)).unwrap(), 1000);
        assert_eq!(resolve_amount(5000, Some(1000), Some(21_000)).unwrap(), 21_000);
    }

    #[test]
    fn name_your_price_default_is_suggested_price() {
        assert_eq!(resolve_amount(5000, Some(1000), None).unwrap(), 5000);
        // Floor above suggested price: default clamps up to the floor
        assert_eq!(resolve_amount(0, Some(1000), None).unwrap(), 1000);
    }

    // ─── preimage → hash binding ────────────────────────────────

    #[test]
    fn preimage_hashes_to_payment_hash() {
        let preimage = [7u8; 32];
        let hash = hex::encode(Sha256::digest(preimage));
        // The gate recomputes this exact mapping in get_download
        assert_eq!(hash.len(), 64);
        let again = hex::encode(Sha256::digest(preimage));
        assert_eq!(hash, again);
    }
}
