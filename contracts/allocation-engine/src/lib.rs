#![no_std]
//! # Allocation Engine
//!
//! Moves vault capital into pool adapters and back, and reports the value of
//! everything deployed (`total_value`), which the vault adds to its
//! `total_assets`.
//!
//! Every adapter speaks the same [`StrategyAdapter`] interface, so the engine
//! is agnostic to what sits behind it (Tenka tkUSD, Etherfuse Stablebonds).
//!
//! Every `allocate` is checked, against the values *after* the move, for:
//!
//! - the **reserve floor**: liquid USDC left on the vault stays at or above
//!   `reserve_floor_bps` of total assets;
//! - the **adapter cap**, the **originator cap** and the **jurisdiction cap**,
//!   each a share of total assets in basis points.
//!
//! Roles: the owner (`stellar-access::ownable`) registers adapters and sets
//! caps; the curator moves capital. Pausing (`stellar-contract-utils`)
//! stops `allocate` only: capital can always come back.
//!
//! No call from the engine leads back into the engine: the vault reads
//! `total_value`, the engine reads the vault's `liquid_assets` (a balance
//! read) and calls `release`, which does not call the engine.

#[cfg(test)]
mod test;

use agama_interfaces::{AllocationEngineView, StrategyAdapterClient, VaultLiquidityClient, BPS};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, Address,
    Env, Map, Symbol, Vec,
};
use stellar_access::ownable::{self, Ownable};
use stellar_contract_utils::pausable::{self, Pausable};
use stellar_macros::{only_owner, when_not_paused};

/// Upper bound on registered adapters, so every loop is bounded.
pub const MAX_ADAPTERS: u32 = 10;

const DAY_IN_LEDGERS: u32 = 17_280;
const TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
const TTL_EXTEND_TO: u32 = 60 * DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum EngineError {
    InvalidAmount = 5000,
    UnknownAdapter = 5001,
    AdapterExists = 5002,
    TooManyAdapters = 5003,
    AssetMismatch = 5004,
    AdapterNotEmpty = 5005,
    InvalidBps = 5006,
    ReserveFloorBreached = 5007,
    AdapterCapExceeded = 5008,
    OriginatorCapExceeded = 5009,
    JurisdictionCapExceeded = 5010,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterInfo {
    pub originator: Symbol,
    pub jurisdiction: Symbol,
    pub cap_bps: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Vault,
    Asset,
    Curator,
    ReserveFloorBps,
    Adapters,
    Adapter(Address),
    OriginatorCap(Symbol),
    JurisdictionCap(Symbol),
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Allocated {
    #[topic]
    pub adapter: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deallocated {
    #[topic]
    pub adapter: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterRegistered {
    #[topic]
    pub adapter: Address,
    pub info: AdapterInfo,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterRemoved {
    #[topic]
    pub adapter: Address,
}

#[contract]
pub struct AllocationEngine;

// ---------------------------------------------------------------- storage helpers

fn bump(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn get<V: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(e: &Env, k: &DataKey) -> Option<V> {
    e.storage().instance().get(k)
}

fn set<V: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(e: &Env, k: &DataKey, v: &V) {
    e.storage().instance().set(k, v);
    bump(e);
}

fn vault(e: &Env) -> Address {
    get(e, &DataKey::Vault).unwrap()
}

fn adapters(e: &Env) -> Vec<Address> {
    get(e, &DataKey::Adapters).unwrap_or_else(|| Vec::new(e))
}

fn info(e: &Env, adapter: &Address) -> AdapterInfo {
    get(e, &DataKey::Adapter(adapter.clone()))
        .unwrap_or_else(|| panic_with_error!(e, EngineError::UnknownAdapter))
}

fn check_bps(e: &Env, bps: u32) {
    if bps as i128 > BPS {
        panic_with_error!(e, EngineError::InvalidBps);
    }
}

fn originator_cap(e: &Env, s: &Symbol) -> u32 {
    get(e, &DataKey::OriginatorCap(s.clone())).unwrap_or(BPS as u32)
}

fn jurisdiction_cap(e: &Env, s: &Symbol) -> u32 {
    get(e, &DataKey::JurisdictionCap(s.clone())).unwrap_or(BPS as u32)
}

/// `value <= cap_bps * total / BPS`, without division.
fn within(value: i128, cap_bps: u32, total: i128) -> bool {
    value * BPS <= cap_bps as i128 * total
}

fn require_curator(e: &Env) {
    let curator: Address = get(e, &DataKey::Curator).unwrap();
    curator.require_auth();
}

#[contractimpl]
impl AllocationEngine {
    pub fn __constructor(
        e: &Env,
        owner: Address,
        curator: Address,
        vault: Address,
        asset: Address,
    ) {
        ownable::set_owner(e, &owner);
        set(e, &DataKey::Curator, &curator);
        set(e, &DataKey::Vault, &vault);
        set(e, &DataKey::Asset, &asset);
        set(e, &DataKey::ReserveFloorBps, &0u32);
    }

    // ------------------------------------------------------------ admin

    #[only_owner]
    pub fn set_curator(e: &Env, curator: Address) {
        set(e, &DataKey::Curator, &curator);
    }

    #[only_owner]
    pub fn set_reserve_floor(e: &Env, bps: u32) {
        check_bps(e, bps);
        set(e, &DataKey::ReserveFloorBps, &bps);
    }

    #[only_owner]
    pub fn register_adapter(
        e: &Env,
        adapter: Address,
        originator: Symbol,
        jurisdiction: Symbol,
        cap_bps: u32,
    ) {
        check_bps(e, cap_bps);
        let key = DataKey::Adapter(adapter.clone());
        if e.storage().instance().has(&key) {
            panic_with_error!(e, EngineError::AdapterExists);
        }
        let mut list = adapters(e);
        if list.len() >= MAX_ADAPTERS {
            panic_with_error!(e, EngineError::TooManyAdapters);
        }
        let asset: Address = get(e, &DataKey::Asset).unwrap();
        if StrategyAdapterClient::new(e, &adapter).asset() != asset {
            panic_with_error!(e, EngineError::AssetMismatch);
        }
        let info = AdapterInfo {
            originator,
            jurisdiction,
            cap_bps,
        };
        set(e, &key, &info);
        list.push_back(adapter.clone());
        set(e, &DataKey::Adapters, &list);
        AdapterRegistered { adapter, info }.publish(e);
    }

    #[only_owner]
    pub fn set_adapter_cap(e: &Env, adapter: Address, cap_bps: u32) {
        check_bps(e, cap_bps);
        let mut i = info(e, &adapter);
        i.cap_bps = cap_bps;
        set(e, &DataKey::Adapter(adapter), &i);
    }

    #[only_owner]
    pub fn set_originator_cap(e: &Env, originator: Symbol, cap_bps: u32) {
        check_bps(e, cap_bps);
        set(e, &DataKey::OriginatorCap(originator), &cap_bps);
    }

    #[only_owner]
    pub fn set_jurisdiction_cap(e: &Env, jurisdiction: Symbol, cap_bps: u32) {
        check_bps(e, cap_bps);
        set(e, &DataKey::JurisdictionCap(jurisdiction), &cap_bps);
    }

    /// Removes an adapter holding nothing.
    #[only_owner]
    pub fn remove_adapter(e: &Env, adapter: Address) {
        info(e, &adapter);
        if StrategyAdapterClient::new(e, &adapter).total_value() != 0 {
            panic_with_error!(e, EngineError::AdapterNotEmpty);
        }
        let mut list = adapters(e);
        if let Some(i) = list.first_index_of(&adapter) {
            list.remove(i);
        }
        set(e, &DataKey::Adapters, &list);
        e.storage()
            .instance()
            .remove(&DataKey::Adapter(adapter.clone()));
        AdapterRemoved { adapter }.publish(e);
    }

    // ------------------------------------------------------------ capital

    /// Moves `amount` of liquid vault USDC into `adapter`. Curator only.
    #[when_not_paused]
    pub fn allocate(e: &Env, adapter: Address, amount: i128) {
        require_curator(e);
        if amount <= 0 {
            panic_with_error!(e, EngineError::InvalidAmount);
        }
        let target = info(e, &adapter);

        // Values before the move.
        let list = adapters(e);
        let mut values: Map<Address, i128> = Map::new(e);
        let mut deployed = 0i128;
        for a in list.iter() {
            let v = StrategyAdapterClient::new(e, &a).total_value();
            deployed += v;
            values.set(a, v);
        }
        let vault_addr = vault(e);
        let vault_client = VaultLiquidityClient::new(e, &vault_addr);
        let liquid = vault_client.liquid_assets();
        let total = liquid + deployed; // unchanged by the move

        // Reserve floor.
        let floor: u32 = get(e, &DataKey::ReserveFloorBps).unwrap_or(0);
        if (liquid - amount) * BPS < floor as i128 * total {
            panic_with_error!(e, EngineError::ReserveFloorBreached);
        }

        // Caps, on post-move values.
        let new_value = values.get(adapter.clone()).unwrap_or(0) + amount;
        if !within(new_value, target.cap_bps, total) {
            panic_with_error!(e, EngineError::AdapterCapExceeded);
        }
        let mut by_originator = amount;
        let mut by_jurisdiction = amount;
        for a in list.iter() {
            let i = info(e, &a);
            let v = values.get(a).unwrap_or(0);
            if i.originator == target.originator {
                by_originator += v;
            }
            if i.jurisdiction == target.jurisdiction {
                by_jurisdiction += v;
            }
        }
        if !within(by_originator, originator_cap(e, &target.originator), total) {
            panic_with_error!(e, EngineError::OriginatorCapExceeded);
        }
        if !within(
            by_jurisdiction,
            jurisdiction_cap(e, &target.jurisdiction),
            total,
        ) {
            panic_with_error!(e, EngineError::JurisdictionCapExceeded);
        }

        vault_client.release(&amount, &adapter);
        StrategyAdapterClient::new(e, &adapter).deposit(&amount);
        Allocated { adapter, amount }.publish(e);
    }

    /// Returns `amount` from `adapter` to the vault. Curator only. Works
    /// while paused.
    pub fn deallocate(e: &Env, adapter: Address, amount: i128) {
        require_curator(e);
        if amount <= 0 {
            panic_with_error!(e, EngineError::InvalidAmount);
        }
        info(e, &adapter);
        let sent = StrategyAdapterClient::new(e, &adapter).withdraw(&amount, &vault(e));
        Deallocated {
            adapter,
            amount: sent,
        }
        .publish(e);
    }

    // ------------------------------------------------------------ views

    pub fn adapters(e: &Env) -> Vec<Address> {
        adapters(e)
    }

    pub fn adapter_info(e: &Env, adapter: Address) -> AdapterInfo {
        info(e, &adapter)
    }

    pub fn vault(e: &Env) -> Address {
        vault(e)
    }

    pub fn curator(e: &Env) -> Address {
        get(e, &DataKey::Curator).unwrap()
    }

    pub fn reserve_floor_bps(e: &Env) -> u32 {
        get(e, &DataKey::ReserveFloorBps).unwrap_or(0)
    }

    pub fn originator_cap(e: &Env, originator: Symbol) -> u32 {
        originator_cap(e, &originator)
    }

    pub fn jurisdiction_cap(e: &Env, jurisdiction: Symbol) -> u32 {
        jurisdiction_cap(e, &jurisdiction)
    }
}

#[contractimpl]
impl AllocationEngineView for AllocationEngine {
    fn total_value(e: Env) -> i128 {
        let mut sum = 0i128;
        for a in adapters(&e).iter() {
            sum += StrategyAdapterClient::new(&e, &a).total_value();
        }
        sum
    }
}

#[contractimpl(contracttrait)]
impl Ownable for AllocationEngine {}

#[contractimpl(contracttrait)]
impl Pausable for AllocationEngine {
    fn paused(e: &Env) -> bool {
        pausable::paused(e)
    }

    #[only_owner]
    fn pause(e: &Env, _caller: Address) {
        pausable::pause(e);
    }

    #[only_owner]
    fn unpause(e: &Env, _caller: Address) {
        pausable::unpause(e);
    }
}
