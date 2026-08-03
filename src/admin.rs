// Operator admin API — NIP-98-gated (same artist key as uploads).
//
// Drives node operations the public gate must never expose: funding
// addresses, peer connects, on-chain sends, and the LSPS1 order flow that
// buys the node its inbound channel at onboarding (bLIP-51; Megalith's
// implementation is REST). The daemon holds the wallet, so it pays for its
// own channel — deposit sats, order, done.

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ldk_node::bitcoin::Address;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::Node;
use nostr::PublicKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

#[derive(Clone)]
pub struct AdminState {
    pub node: Arc<Node>,
    /// Operator key authorized for admin actions (balance, on-chain spends,
    /// LSPS1 orders). Deliberately separate from the artist's Nostr
    /// publishing identity — signing music must not authorize spending.
    pub admin_pubkey: Option<PublicKey>,
    pub public_url: String,
    pub network: ldk_node::bitcoin::Network,
    /// LSPS1 REST base, e.g. https://megalithic.me/api/lsps1/v1
    pub lsps1_api_url: Option<String>,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Verify the NIP-98 header for an admin request. Body hash is enforced
/// whenever a body is present.
fn authorize(
    state: &AdminState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<(), Response> {
    let Some(ref admin_pk) = state.admin_pubkey else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Admin API disabled: ADMIN_PUBKEY not configured",
        ));
    };
    let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return Err(err(StatusCode::UNAUTHORIZED, "Missing Authorization header"));
    };
    let url = format!("{}{}", state.public_url.trim_end_matches('/'), path);
    let body_hash = body.map(|b| hex::encode(Sha256::digest(b)));
    crate::nip98::verify(auth, method, &url, body_hash.as_deref(), admin_pk, now_secs())
        .map(|_| ())
        .map_err(|e| err(StatusCode::UNAUTHORIZED, format!("NIP-98: {e}")))
}

/// Parse "pubkey@host:port" into ldk types.
pub fn parse_node_uri(
    uri: &str,
) -> Result<(ldk_node::bitcoin::secp256k1::PublicKey, SocketAddress), String> {
    let (pk, addr) = uri
        .split_once('@')
        .ok_or("Node URI must be pubkey@host:port")?;
    let node_id = pk.parse().map_err(|e| format!("Invalid pubkey: {e}"))?;
    let address: SocketAddress = addr
        .parse()
        .map_err(|_| format!("Invalid address: {addr}"))?;
    Ok((node_id, address))
}

/// Build the bLIP-51 create_order request body. Pure — testable.
pub fn build_create_order_body(
    node_id: &str,
    lsp_balance_sat: u64,
    channel_expiry_blocks: u32,
) -> serde_json::Value {
    serde_json::json!({
        "public_key": node_id,
        "lsp_balance_sat": lsp_balance_sat.to_string(),
        "client_balance_sat": "0",
        "required_channel_confirmations": 0,
        "funding_confirms_within_blocks": 6,
        "channel_expiry_blocks": channel_expiry_blocks,
        "announce_channel": false,
    })
}

// ─── Handlers ────────────────────────────────────────────────

pub async fn get_address(
    AxumState(state): AxumState<AdminState>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize(&state, &headers, "GET", "/admin/address", None) {
        return resp;
    }
    match state.node.onchain_payment().new_address() {
        Ok(addr) => Json(serde_json::json!({ "address": addr.to_string() })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    }
}

pub async fn get_balance(
    AxumState(state): AxumState<AdminState>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize(&state, &headers, "GET", "/admin/balance", None) {
        return resp;
    }
    let b = state.node.list_balances();
    let inbound_msat: u64 = state
        .node
        .list_channels()
        .iter()
        .filter(|c| c.is_usable)
        .map(|c| c.inbound_capacity_msat)
        .sum();
    Json(serde_json::json!({
        "spendable_onchain_sats": b.spendable_onchain_balance_sats,
        "total_onchain_sats": b.total_onchain_balance_sats,
        "lightning_sats": b.total_lightning_balance_sats,
        "inbound_capacity_sats": inbound_msat / 1000,
        "channels": state.node.list_channels().len(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ConnectRequest {
    uri: String,
}

pub async fn post_connect(
    AxumState(state): AxumState<AdminState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = authorize(&state, &headers, "POST", "/admin/connect", Some(&body)) {
        return resp;
    }
    let req: ConnectRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("Invalid body: {e}")),
    };
    let (node_id, address) = match parse_node_uri(&req.uri) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };

    let node = state.node.clone();
    let result = tokio::task::spawn_blocking(move || node.connect(node_id, address, true)).await;
    match result {
        Ok(Ok(())) => {
            info!(uri = %req.uri, "Admin: connected to peer");
            Json(serde_json::json!({ "connected": true })).into_response()
        }
        Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, format!("Connect failed: {e}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("Task failed: {e}")),
    }
}

#[derive(Deserialize)]
struct SendOnchainRequest {
    address: String,
    amount_sats: u64,
}

pub async fn post_send_onchain(
    AxumState(state): AxumState<AdminState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = authorize(&state, &headers, "POST", "/admin/send-onchain", Some(&body)) {
        return resp;
    }
    let req: SendOnchainRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("Invalid body: {e}")),
    };
    let address = match Address::from_str(&req.address)
        .map_err(|e| format!("Invalid address: {e}"))
        .and_then(|a| {
            a.require_network(state.network)
                .map_err(|_| format!("Address is not valid for {}", state.network))
        }) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };

    match state
        .node
        .onchain_payment()
        .send_to_address(&address, req.amount_sats, None)
    {
        Ok(txid) => {
            info!(txid = %txid, amount_sats = req.amount_sats, "Admin: on-chain send");
            Json(serde_json::json!({ "txid": txid.to_string() })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("Send failed: {e}")),
    }
}

#[derive(Deserialize)]
struct Lsps1OrderRequest {
    lsp_balance_sat: u64,
    /// Defaults to ~3 months if unset.
    channel_expiry_blocks: Option<u32>,
}

/// Create an LSPS1 order with the configured provider. Returns the raw
/// bLIP-51 order JSON (order_id + payment options) — callers extract what
/// they need; we stay schema-agnostic across providers.
pub async fn post_lsps1_order(
    AxumState(state): AxumState<AdminState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = authorize(&state, &headers, "POST", "/admin/lsps1-order", Some(&body)) {
        return resp;
    }
    let Some(ref api) = state.lsps1_api_url else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "LSPS1_API_URL not configured",
        );
    };
    let req: Lsps1OrderRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("Invalid body: {e}")),
    };

    let order_body = build_create_order_body(
        &state.node.node_id().to_string(),
        req.lsp_balance_sat,
        req.channel_expiry_blocks.unwrap_or(13_000),
    );

    let url = format!("{}/create_order", api.trim_end_matches('/'));
    let resp = match reqwest::Client::new()
        .post(&url)
        .json(&order_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("LSPS1 unreachable: {e}")),
    };

    let status = resp.status();
    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("LSPS1 bad response: {e}")),
    };
    if !status.is_success() {
        return err(
            StatusCode::BAD_GATEWAY,
            format!("LSPS1 create_order {}: {}", status, json),
        );
    }
    info!(lsp_balance_sat = req.lsp_balance_sat, "Admin: LSPS1 order created");
    Json(json).into_response()
}

/// Fetch order status from the LSPS1 provider (payment/channel state).
pub async fn get_lsps1_order(
    AxumState(state): AxumState<AdminState>,
    AxumPath(order_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/admin/lsps1-order/{order_id}");
    if let Err(resp) = authorize(&state, &headers, "GET", &path, None) {
        return resp;
    }
    let Some(ref api) = state.lsps1_api_url else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "LSPS1_API_URL not configured",
        );
    };
    let url = format!(
        "{}/get_order?order_id={}",
        api.trim_end_matches('/'),
        order_id
    );
    match reqwest::Client::new().get(&url).send().await {
        Ok(r) => {
            let status = r.status();
            match r.json::<serde_json::Value>().await {
                Ok(v) if status.is_success() => Json(v).into_response(),
                Ok(v) => err(StatusCode::BAD_GATEWAY, format!("LSPS1 get_order {}: {}", status, v)),
                Err(e) => err(StatusCode::BAD_GATEWAY, format!("LSPS1 bad response: {e}")),
            }
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("LSPS1 unreachable: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_uri_parses() {
        let (pk, _addr) = parse_node_uri(
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798@203.0.113.7:9735",
        )
        .unwrap();
        assert_eq!(
            pk.to_string(),
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
        assert!(parse_node_uri("no-at-sign").is_err());
        assert!(parse_node_uri("deadbeef@host:9735").is_err());
    }

    #[test]
    fn create_order_body_is_blip51_shaped() {
        let body = build_create_order_body("02aa", 500_000, 13_000);
        assert_eq!(body["public_key"], "02aa");
        assert_eq!(body["lsp_balance_sat"], "500000"); // string per bLIP-51
        assert_eq!(body["client_balance_sat"], "0");
        assert_eq!(body["required_channel_confirmations"], 0);
        assert_eq!(body["announce_channel"], false);
    }
}
