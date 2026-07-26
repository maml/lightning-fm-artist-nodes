// Lightning FM — Headless Artist Node
//
// Minimal LDK node for managed artists. Runs in Docker, receives
// keysend streaming payments, exposes a health + invoice HTTP API.
//
// Usage:
//   LDK_MNEMONIC="abandon ... about" ARTIST_NAME="Satoshi Sounds" lfm-artist-node

mod gate;
mod nip98;
mod store;

use ldk_node::bip39::Mnemonic;
use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Event, Node};

use axum::extract::DefaultBodyLimit;
use axum::{extract::State as AxumState, extract::Query, routing::{get, post, put}, Json, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::signal;
use tracing::{error, info, warn};

use store::Store;

// ---------------------------------------------------------------------------
// Config from environment
// ---------------------------------------------------------------------------

struct Config {
    artist_name: String,
    mnemonic: Mnemonic,
    network: Network,
    data_dir: String,
    esplora_url: String,
    /// http://user:pass@host:port — when set, used as the chain source
    /// instead of Esplora (regtest dev-cluster path).
    bitcoind_rpc_url: Option<String>,
    rgs_url: Option<String>,
    lsp_node_id: String,
    lsp_address: String,
    lsp_token: Option<String>,
    health_port: u16,
    /// Hex pubkey allowed to upload product artifacts (NIP-98 signer).
    artist_pubkey: Option<String>,
    /// External base URL of this daemon, for NIP-98 URL verification.
    public_url: String,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let mnemonic_str = std::env::var("LDK_MNEMONIC")
            .map_err(|_| "LDK_MNEMONIC env var not set")?;
        // Note: not clearing env var here to avoid unsafe. Acceptable for signet.

        let mnemonic: Mnemonic = mnemonic_str.parse()
            .map_err(|e| format!("Invalid mnemonic: {e}"))?;

        let network = match std::env::var("NETWORK").ok().as_deref() {
            Some("regtest") => Network::Regtest,
            Some("testnet") => Network::Testnet,
            Some("bitcoin") | Some("mainnet") => Network::Bitcoin,
            _ => Network::Signet,
        };

        let health_port = std::env::var("HEALTH_PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse().unwrap_or(8080);

        Ok(Config {
            artist_name: std::env::var("ARTIST_NAME").unwrap_or_else(|_| "Unknown Artist".to_string()),
            mnemonic,
            network,
            data_dir: std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string()),
            esplora_url: std::env::var("ESPLORA_URL")
                .unwrap_or_else(|_| "https://mutinynet.com/api".to_string()),
            bitcoind_rpc_url: std::env::var("BITCOIND_RPC_URL").ok(),
            rgs_url: std::env::var("RGS_URL").ok(),
            lsp_node_id: std::env::var("LSP_NODE_ID").unwrap_or_else(|_|
                "0371d6fd7d75de2d0372d03ea00e8bacdacb50c27d0eaea0a76a0622eff1f5ef2b".to_string()),
            lsp_address: std::env::var("LSP_ADDRESS")
                .unwrap_or_else(|_| "44.228.24.253:9735".to_string()),
            lsp_token: std::env::var("LSP_TOKEN").ok(),
            health_port,
            artist_pubkey: std::env::var("ARTIST_PUBKEY").ok(),
            public_url: std::env::var("PUBLIC_URL")
                .unwrap_or_else(|_| format!("http://localhost:{}", health_port)),
        })
    }
}

/// Parse http://user:pass@host:port into (host, port, user, pass).
fn parse_bitcoind_rpc(url: &str) -> Result<(String, u16, String, String), String> {
    let without_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or("BITCOIND_RPC_URL must start with http(s)://")?;
    let (creds, host_port) = without_scheme.split_once('@')
        .ok_or("BITCOIND_RPC_URL missing user:pass@")?;
    let (user, pass) = creds.split_once(':')
        .ok_or("BITCOIND_RPC_URL missing user:pass")?;
    let (host, port_str) = host_port.split_once(':')
        .ok_or("BITCOIND_RPC_URL missing host:port")?;
    let port: u16 = port_str.parse()
        .map_err(|_| "BITCOIND_RPC_URL has an invalid port")?;
    Ok((host.to_string(), port, user.to_string(), pass.to_string()))
}

// ---------------------------------------------------------------------------
// Node setup
// ---------------------------------------------------------------------------

fn build_node(config: &Config) -> Result<Node, String> {
    let mut builder = Builder::new();

    builder.set_network(config.network);
    builder.set_entropy_bip39_mnemonic(config.mnemonic.clone(), None);
    builder.set_storage_dir_path(config.data_dir.clone());

    match config.bitcoind_rpc_url {
        Some(ref url) => {
            let (host, port, user, pass) = parse_bitcoind_rpc(url)?;
            builder.set_chain_source_bitcoind_rpc(host, port, user, pass);
        }
        None => {
            builder.set_chain_source_esplora(config.esplora_url.clone(), None);
        }
    }

    if let Some(ref rgs_url) = config.rgs_url {
        builder.set_gossip_source_rgs(rgs_url.clone());
    }

    // LSPS2 liquidity source — LSP opens JIT channels on invoice payment
    let lsp_pubkey = config.lsp_node_id.parse()
        .map_err(|e| format!("Invalid LSP node_id: {e}"))?;
    let lsp_addr: SocketAddress = config.lsp_address.parse()
        .map_err(|e| format!("Invalid LSP address: {e}"))?;
    builder.set_liquidity_source_lsps2(lsp_pubkey, lsp_addr, config.lsp_token.clone());

    // No listening address — we only connect outbound to the LSP
    // Payments route through the LSP channel, no inbound TCP needed

    let node = builder.build()
        .map_err(|e| format!("Failed to build LDK node: {e}"))?;

    Ok(node)
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

async fn run_event_loop(node: Arc<Node>, artist_name: String, store: Arc<Mutex<Store>>) {
    info!("Event loop started for {}", artist_name);

    loop {
        let event = node.next_event_async().await;

        match &event {
            Event::PaymentReceived {
                payment_hash,
                amount_msat,
                custom_records,
                ..
            } => {
                let amount_sats = amount_msat / 1000;
                info!(
                    artist = %artist_name,
                    amount_sats = amount_sats,
                    amount_msat = amount_msat,
                    payment_hash = %payment_hash,
                    "Payment received"
                );

                // Settle any pending purchase carrying this hash — the
                // streaming keysend path shares this node, so misses are fine.
                match store.lock() {
                    Ok(mut s) => match s.mark_paid(&payment_hash.to_string()) {
                        Ok(true) => info!(payment_hash = %payment_hash, "Purchase settled"),
                        Ok(false) => {}
                        Err(e) => error!(error = %e, "Failed to persist purchase settlement"),
                    },
                    Err(_) => error!("Store lock poisoned in event loop"),
                }

                // Log custom TLV records (track_id, listener_pubkey, timestamp)
                for record in custom_records.iter() {
                    match record.type_num {
                        696969 => {
                            if let Ok(track_id) = String::from_utf8(record.value.clone()) {
                                info!(track_id = %track_id, "TLV: track_id");
                            }
                        }
                        696971 => {
                            if let Ok(pubkey) = String::from_utf8(record.value.clone()) {
                                info!(listener_pubkey = %pubkey, "TLV: listener_pubkey");
                            }
                        }
                        696973 => {
                            if let Ok(ts) = String::from_utf8(record.value.clone()) {
                                info!(timestamp = %ts, "TLV: timestamp");
                            }
                        }
                        _ => {
                            info!(tlv_type = record.type_num, "TLV: unknown type");
                        }
                    }
                }
            }

            Event::ChannelPending { channel_id, counterparty_node_id, .. } => {
                info!(
                    artist = %artist_name,
                    channel_id = %channel_id,
                    peer = %counterparty_node_id,
                    "Channel pending"
                );
            }

            Event::ChannelReady { channel_id, counterparty_node_id, .. } => {
                info!(
                    artist = %artist_name,
                    channel_id = %channel_id,
                    peer = ?counterparty_node_id,
                    "Channel ready"
                );
            }

            Event::ChannelClosed { channel_id, reason, .. } => {
                warn!(
                    artist = %artist_name,
                    channel_id = %channel_id,
                    reason = ?reason,
                    "Channel closed"
                );
            }

            Event::PaymentSuccessful { payment_hash, .. } => {
                info!(artist = %artist_name, payment_hash = %payment_hash, "Payment sent successfully");
            }

            Event::PaymentFailed { payment_hash, reason, .. } => {
                warn!(artist = %artist_name, payment_hash = ?payment_hash, reason = ?reason, "Payment failed");
            }

            _ => {
                info!(artist = %artist_name, event = ?event, "LDK event");
            }
        }

        if let Err(e) = node.event_handled() {
            error!("event_handled() failed: {:?} — stopping event loop", e);
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP management API
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    node: Arc<Node>,
    artist_name: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    artist: String,
    node_id: String,
    network: String,
    channels: usize,
    peers: usize,
}

#[derive(Serialize)]
struct InvoiceResponse {
    bolt11: String,
    amount_sats: u64,
    expiry_secs: u32,
}

#[derive(Serialize)]
struct NodeIdResponse {
    node_id: String,
    artist: String,
}

#[derive(Deserialize)]
struct InvoiceParams {
    amount_sats: Option<u64>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

async fn health(AxumState(state): AxumState<AppState>) -> Json<HealthResponse> {
    let node_id = state.node.node_id().to_string();
    let channels = state.node.list_channels().len();
    let peers = state.node.list_peers().len();

    Json(HealthResponse {
        status: "ok".to_string(),
        artist: state.artist_name.clone(),
        node_id,
        network: "signet".to_string(),
        channels,
        peers,
    })
}

async fn get_node_id(AxumState(state): AxumState<AppState>) -> Json<NodeIdResponse> {
    Json(NodeIdResponse {
        node_id: state.node.node_id().to_string(),
        artist: state.artist_name.clone(),
    })
}

async fn create_invoice(
    AxumState(state): AxumState<AppState>,
    Query(params): Query<InvoiceParams>,
) -> Result<Json<InvoiceResponse>, Json<ErrorResponse>> {
    let amount_sats = params.amount_sats.unwrap_or(50_000);
    let amount_msat = amount_sats * 1000;
    let expiry_secs = 3600; // 1 hour
    let description = format!("Lightning FM — {} bootstrap", state.artist_name);

    let desc = ldk_node::lightning_invoice::Description::new(description.clone())
        .map_err(|_| Json(ErrorResponse { error: "Invalid description".to_string() }))?;
    let invoice_desc = ldk_node::lightning_invoice::Bolt11InvoiceDescription::Direct(desc);

    match state.node.bolt11_payment().receive(amount_msat, &invoice_desc, expiry_secs) {
        Ok(invoice) => {
            info!(
                artist = %state.artist_name,
                amount_sats = amount_sats,
                "Invoice created for channel bootstrap"
            );
            Ok(Json(InvoiceResponse {
                bolt11: invoice.to_string(),
                amount_sats,
                expiry_secs,
            }))
        }
        Err(e) => {
            error!(artist = %state.artist_name, error = ?e, "Failed to create invoice");
            Err(Json(ErrorResponse {
                error: format!("Failed to create invoice: {e:?}"),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Early stdout to confirm binary starts (before tracing init)
    eprintln!("[lfm-artist-node] Starting...");

    // Initialize structured logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ldk_node=warn".parse().unwrap()),
        )
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            error!("Configuration error: {}", e);
            std::process::exit(1);
        }
    };

    info!(
        artist = %config.artist_name,
        network = "signet",
        data_dir = %config.data_dir,
        "Starting headless artist node"
    );

    // Build and start the node
    let node = match build_node(&config) {
        Ok(n) => n,
        Err(e) => {
            error!("Failed to build node: {}", e);
            std::process::exit(1);
        }
    };

    // Retry start — Esplora fee estimation can timeout on cold start
    let mut started = false;
    for attempt in 1..=3 {
        match node.start() {
            Ok(()) => {
                started = true;
                break;
            }
            Err(e) => {
                warn!("Node start attempt {}/3 failed: {}. Retrying in 5s...", attempt, e);
                if attempt < 3 {
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }
    if !started {
        error!("Failed to start node after 3 attempts");
        std::process::exit(1);
    }

    let node_id = node.node_id();
    info!(
        artist = %config.artist_name,
        node_id = %node_id,
        "Node started"
    );

    // Wait for chain sync
    info!("Waiting for chain sync...");
    tokio::time::sleep(Duration::from_secs(5)).await;
    info!("Node synced and ready");

    let node = Arc::new(node);

    // Product store (registry + purchase ledger)
    let store = match Store::open(&config.data_dir) {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            error!("Failed to open product store: {}", e);
            std::process::exit(1);
        }
    };

    // Artist pubkey for NIP-98 upload auth (uploads disabled when unset)
    let artist_pubkey = match config.artist_pubkey.as_deref() {
        Some(hex) => match hex.parse::<nostr::PublicKey>() {
            Ok(pk) => Some(pk),
            Err(e) => {
                error!("Invalid ARTIST_PUBKEY: {}", e);
                std::process::exit(1);
            }
        },
        None => {
            warn!("ARTIST_PUBKEY not set — product uploads disabled");
            None
        }
    };

    // Start event loop
    let event_node = node.clone();
    let event_artist = config.artist_name.clone();
    let event_store = store.clone();
    let event_handle = tokio::spawn(async move {
        run_event_loop(event_node, event_artist, event_store).await;
    });

    // Start HTTP management API
    let app_state = AppState {
        node: node.clone(),
        artist_name: config.artist_name.clone(),
    };

    let gate_state = gate::GateState {
        node: node.clone(),
        store,
        artist_name: config.artist_name.clone(),
        artist_pubkey,
        public_url: config.public_url.clone(),
    };

    let gate_routes = Router::new()
        .route("/products/{slug}", put(gate::put_product).get(gate::get_product))
        .route("/products/{slug}/invoice", post(gate::post_invoice))
        .route("/products/{slug}/download", get(gate::get_download))
        // Lossless audio artifacts run large — WAV albums can exceed 1 GB
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024))
        .with_state(gate_state);

    let app = Router::new()
        .route("/health", get(health))
        .route("/node-id", get(get_node_id))
        .route("/invoice", get(create_invoice))
        .with_state(app_state)
        .merge(gate_routes);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.health_port));
    info!(port = config.health_port, "HTTP management API listening");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    // Run HTTP server + event loop, shut down on SIGTERM/SIGINT
    tokio::select! {
        result = axum::serve(listener, app) => {
            if let Err(e) = result {
                error!("HTTP server error: {}", e);
            }
        }
        _ = event_handle => {
            warn!("Event loop exited unexpectedly");
        }
        _ = signal::ctrl_c() => {
            info!(artist = %config.artist_name, "Shutting down...");
        }
    }

    // Graceful shutdown
    info!("Stopping LDK node...");
    if let Err(e) = node.stop() {
        error!("Failed to stop node: {:?}", e);
    } else {
        info!(artist = %config.artist_name, "Node stopped cleanly");
    }
}
