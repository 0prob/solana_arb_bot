// src/tatum/mod.rs
// ═══════════════════════════════════════════════════════════════════════
//  Tatum gRPC Provider
//
//  Tatum exposes Solana's Yellowstone/Geyser gRPC streaming at their
//  managed gateway endpoint. Auth is via `x-api-key` metadata header,
//  which maps directly to the existing `grpc_x_token` mechanism in
//  `GeyserGrpcClient`.
//
//  Endpoint format:
//    solana-mainnet.gateway.tatum.io:443
//
//  Auth header (x-token for yellowstone-grpc-client):
//    x-api-key: <TATUM_API_KEY>
//
//  This module provides:
//    - `TatumGrpcConfig`  — resolved connection parameters for Tatum
//    - `build_endpoint`   — constructs the full gRPC URI from Tatum config
//    - `TatumGrpcProvider`— thin wrapper that can be used by the listener
//      as a drop-in alternative to self-hosted Yellowstone endpoints
//
//  Usage in listener:
//    The listener's `run()` accepts a `GrpcProvider` enum.  When the
//    `Tatum` variant is selected, this module supplies the endpoint URI
//    and API key so the rest of the listener pipeline (subscription,
//    transaction parsing, etc.) operates identically.
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use tracing::info;

// ── Constants ────────────────────────────────────────────────────────

/// Tatum's Solana mainnet Yellowstone gRPC gateway.
/// Port 443 uses TLS (required by Tatum's managed infrastructure).
pub const TATUM_SOLANA_GRPC_ENDPOINT: &str = "https://solana-mainnet.gateway.tatum.io";

// ── Config ───────────────────────────────────────────────────────────

/// Resolved connection parameters for a Tatum gRPC session.
#[derive(Debug, Clone)]
pub struct TatumGrpcConfig {
    /// Full gRPC endpoint URI, e.g. `https://solana-mainnet.gateway.tatum.io`
    pub endpoint: String,
    /// Tatum API key passed as the `x-api-key` / x-token header.
    /// Contains a secret — never log this field.
    pub api_key: String,
}

impl TatumGrpcConfig {
    /// Construct from explicit values.  Validates that neither field is empty.
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let api_key = api_key.into();

        if endpoint.trim().is_empty() {
            anyhow::bail!("Tatum gRPC endpoint must not be empty");
        }
        if api_key.trim().is_empty() {
            anyhow::bail!("TATUM_API_KEY must not be empty");
        }

        Ok(Self { endpoint, api_key })
    }

    /// Convenience constructor that uses the canonical Tatum Solana mainnet
    /// endpoint and only requires the API key.
    pub fn mainnet(api_key: impl Into<String>) -> Result<Self> {
        Self::new(TATUM_SOLANA_GRPC_ENDPOINT, api_key)
    }
}

// ── Provider ─────────────────────────────────────────────────────────

/// Describes which gRPC backend the listener should connect to.
///
/// Both variants ultimately connect via `yellowstone-grpc-client`; the
/// distinction is where the endpoint lives and how auth is supplied.
#[derive(Debug, Clone)]
pub enum GrpcProvider {
    /// Self-hosted or third-party Yellowstone node.
    /// `endpoint` is the full URI; `x_token` may be empty.
    Custom {
        endpoint: String,
        x_token: String,
    },
    /// Tatum managed Solana gRPC gateway.
    Tatum(TatumGrpcConfig),
}

impl GrpcProvider {
    /// Endpoint URI to pass to `GeyserGrpcClient::build_from_shared`.
    pub fn endpoint(&self) -> &str {
        match self {
            GrpcProvider::Custom { endpoint, .. } => endpoint.as_str(),
            GrpcProvider::Tatum(cfg) => cfg.endpoint.as_str(),
        }
    }

    /// Optional auth token; maps to the `x-token` gRPC metadata header used
    /// by `yellowstone-grpc-client`.  For Tatum this is the API key.
    pub fn x_token(&self) -> Option<String> {
        match self {
            GrpcProvider::Custom { x_token, .. } => {
                if x_token.is_empty() {
                    None
                } else {
                    Some(x_token.clone())
                }
            }
            GrpcProvider::Tatum(cfg) => Some(cfg.api_key.clone()),
        }
    }

    /// Human-readable label for log output (never includes the secret).
    pub fn label(&self) -> String {
        match self {
            GrpcProvider::Custom { endpoint, .. } => format!("custom({})", endpoint),
            GrpcProvider::Tatum(cfg) => {
                // Show only the host; the API key must not appear in logs.
                format!("tatum({})", cfg.endpoint)
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Build a `GrpcProvider` from `AppConfig` fields.
///
/// If `tatum_api_key` is non-empty, the Tatum provider is returned and
/// `tatum_grpc_endpoint` is used (defaulting to the canonical mainnet URI).
/// Otherwise falls back to the custom `grpc_endpoint` + `grpc_x_token`.
pub fn provider_from_config(
    grpc_endpoint: &str,
    grpc_x_token: &str,
    tatum_api_key: &str,
    tatum_grpc_endpoint: &str,
) -> Result<GrpcProvider> {
    if !tatum_api_key.trim().is_empty() {
        let ep = if tatum_grpc_endpoint.trim().is_empty() {
            TATUM_SOLANA_GRPC_ENDPOINT.to_string()
        } else {
            tatum_grpc_endpoint.to_string()
        };
        let cfg = TatumGrpcConfig::new(ep, tatum_api_key)
            .context("Building Tatum gRPC config")?;
        info!(endpoint = %cfg.endpoint, "Using Tatum gRPC provider");
        Ok(GrpcProvider::Tatum(cfg))
    } else {
        info!(endpoint = %grpc_endpoint, "Using custom gRPC provider");
        Ok(GrpcProvider::Custom {
            endpoint: grpc_endpoint.to_string(),
            x_token: grpc_x_token.to_string(),
        })
    }
}
