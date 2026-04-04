// src/dex_registry/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Canonical registry of Solana DEX program IDs.
//
// Jupiter routes reference these by program_id in their route_plan[].
// The listener uses a subset (those we can detect pool creation for)
// to subscribe to gRPC transaction filters.
//
// The registry is built once at first access (OnceLock) and returned as
// a &'static slice thereafter — zero allocations on the hot path.
//
// O(1) lookup map: `dex_lookup_map()` returns a DashMap<Pubkey, &'static str>
// built once from the registry. Use this on the hot path instead of the
// O(n) `label_for_program()` scan.
//
// Important invariant:
// Every entry must be a real, parseable mainnet program ID.
// Do not use placeholders. If a program ID is unknown, omit the entry
// until it can be verified.
// ═══════════════════════════════════════════════════════════════════════

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::OnceLock;

/// A known DEX with its mainnet program ID and human label.
#[derive(Debug, Clone)]
pub struct DexEntry {
    pub program_id: Pubkey,
    pub label: &'static str,
    /// Whether we can detect new pool creation via gRPC tx filter.
    /// If false, we rely on Jupiter routing to discover liquidity,
    /// but we won't get early-bird migration events.
    pub detectable: bool,
}

static ALL_DEXES: OnceLock<Vec<DexEntry>> = OnceLock::new();

/// All DEXes the bot is aware of, built once and cached for the process
/// lifetime. Ordered roughly by relevance / migration priority.
pub fn all_dexes() -> &'static [DexEntry] {
    ALL_DEXES.get_or_init(|| {
        vec![
            // ── Tier 1: High-volume migration targets ───────────────────
            dex("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", "PumpSwap",              true),
            dex("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", "Raydium V4",            true),
            dex("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", "Raydium CLMM",          true),
            dex("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C", "Raydium CPMM",          true),
            dex("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  "Orca Whirlpool",        true),
            dex("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB", "Meteora",               true),
            dex("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",  "Meteora DLMM",          true),
            dex("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  "Pump.fun",              true),

            // ── Tier 2: Active detectable DEXes ─────────────────────────
            dex("swapFpHZwjELNnjvThjajtiVmkz3yPQEHjLtka2fwHW",  "Stabble Weighted",      true),
            dex("swapNyd8XiQwJ6ianp9snpu4brUqFxadzvHebnAXjJZ",  "Stabble Stable",        true),
            dex("tuna4uSQZncNeeiAMKbstuxA9CUkHH6HmC64wgmnogD",  "DefiTuna",              true),
            dex("FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSrm1X", "FluxBeam",              true),
            dex("DSwpgjMvXhtGn6BsbqmacdBZyfLj6jSWf3HJpdJtmg6N", "Dexlab",                true),
            dex("GAMMA7meSFWaBXF25oSUgmGRwaW6sCMFLmBNiMSdbHVT", "GooseFX GAMMA",         true),
            dex("5jnapfrAN47UYkLkEf7HnprPPBCQLvkYWGZDeKkaP5hv", "Daos.fun",              true),
            dex("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX",  "OpenBook V1",           true),
            dex("opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb",  "OpenBook V2",           true),
            dex("HVNunQ7ybEaaPwssYVGeDJgk1R7i5vSBETcQM9K6SHWZ", "Heaven",                true),

            // ── Tier 3: Newly added detectable DEXes ────────────────────
            // Aldrin V2 AMM — active liquidity, emits pool creation events
            dex("CURVGoZn8zycx6FXwwevgBTB2gVvdbGTEpvMJDbgs2t4", "Aldrin V2",             true),
            // Crema Finance — concentrated liquidity pools
            dex("6MLxLqiXaaSUpkgMnWDTuejNZEz3kE7k2woyHGVFw319", "Crema",                 true),
            // Invariant — CLMM, active on Solana mainnet
            dex("HyaB3W9q6XdA5xwpU4XnSZV94htfmbmqJXZcEbRaJutt", "Invariant",             true),
            // Lifinity V1 — proactive market maker
            dex("EewxydAPCCVuNEyrVN68PuSYdQ7wKn27V9Gjeoi8dy3S", "Lifinity V1",           true),
            // Cropper Finance — AMM with farming
            dex("CTMAxxk34HjKWxQ3QLZK1HpaLXmBveao3ESePXbiyfzh", "Cropper",               true),
            // Step Finance — step-n-swap AMM
            dex("SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ",  "StepN Swap",            true),
            // Saros AMM
            dex("SSwapUtytfBdBn1b9NUGG6foMVPtcWgpRU32HToDUZr",  "Saros",                 true),
            // Penguin Finance
            dex("PSwapMdSai8tjrEXcxFeQth87xC4rRsa4VA5mhGhXkP",  "Penguin",               true),
            // Sencha Exchange
            dex("SCHAtsf8mbjyjiv4LkhLKutTf6JnZAbdze7dFpwTGZj",  "Sencha",                true),

            // ── Jupiter-routed only (route passthrough, no pool detection) ──
            dex("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY",  "Phoenix",               false),
            dex("2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c", "Lifinity V2",           false),
            dex("5ocnV1qiCgaQR8Jb8xWnVbApfaygJ8tNoZfgPwsgx9kx", "Sanctum Infinity",      false),
            dex("stkitrT1Uoy18Dk1fTrgPw8W6MVzoCfYoAFT4MLsmhq",  "Sanctum",               false),
            dex("SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe",  "SolFi",                 false),
            dex("MoonCVVNZFSYkqNXP6bxHLPL6QQJiMagDL3qcqUQTrG",  "Moonshot",              false),
            // Token-2022 swap programs — routed via Jupiter only
            dex("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",  "Token-2022",            false),
        ]
    })
}

/// Return only the DEXes that have `detectable = true`.
/// Cached statically after first call — no allocation.
pub fn detectable_dexes() -> &'static [DexEntry] {
    static DETECTABLE: OnceLock<Vec<DexEntry>> = OnceLock::new();
    DETECTABLE.get_or_init(|| {
        all_dexes().iter().filter(|d| d.detectable).cloned().collect()
    })
}

/// O(1) DashMap lookup: program_id → label.
///
/// Built once from `all_dexes()` and cached for the process lifetime.
/// Use this on the hot path (listener transaction processing) instead of
/// the O(n) `label_for_program()` linear scan.
pub fn dex_lookup_map() -> &'static DashMap<Pubkey, &'static str> {
    static MAP: OnceLock<DashMap<Pubkey, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let map = DashMap::with_capacity(all_dexes().len());
        for entry in all_dexes() {
            map.insert(entry.program_id, entry.label);
        }
        map
    })
}

/// O(1) DashMap lookup: program_id → label (detectable only).
///
/// Built once from `detectable_dexes()` and cached for the process lifetime.
/// Used by the listener's hot-path instruction scanner.
pub fn detectable_dex_map() -> &'static DashMap<Pubkey, &'static str> {
    static MAP: OnceLock<DashMap<Pubkey, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let map = DashMap::with_capacity(detectable_dexes().len());
        for entry in detectable_dexes() {
            map.insert(entry.program_id, entry.label);
        }
        map
    })
}

/// Look up a DEX label by program ID (used to classify Jupiter route legs).
/// O(1) via the cached DashMap.
#[allow(dead_code)]
pub fn label_for_program(program_id: &Pubkey) -> Option<&'static str> {
    dex_lookup_map().get(program_id).map(|r| *r.value())
}

fn dex(addr: &str, label: &'static str, detectable: bool) -> DexEntry {
    let program_id = Pubkey::from_str(addr)
        .unwrap_or_else(|_| panic!("Invalid hard-coded DEX address for {label}: {addr}"));
    DexEntry { program_id, label, detectable }
}
