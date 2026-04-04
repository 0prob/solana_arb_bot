use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::OnceLock;

pub fn detectable_dex_map() -> &'static DashMap<Pubkey, &'static str> {
    static MAP: OnceLock<DashMap<Pubkey, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let map = DashMap::new();
        let dexes = [
            ("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", "PumpSwap"),
            ("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", "Raydium V4"),
            ("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", "Raydium CLMM"),
            ("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C", "Raydium CPMM"),
            ("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  "Orca Whirlpool"),
            ("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB", "Meteora"),
            ("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",  "Meteora DLMM"),
            ("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  "Pump.fun"),
        ];
        for (addr, label) in dexes {
            map.insert(Pubkey::from_str(addr).unwrap(), label);
        }
        map
    })
}
