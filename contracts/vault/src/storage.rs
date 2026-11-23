//! Agama-specific storage. Token balances, allowances, supply, the vault
//! asset, ownership and the pause flag live in the OpenZeppelin modules.

use soroban_sdk::{contracttype, Address, Env};

use crate::queue::RedeemRequest;

const DAY_IN_LEDGERS: u32 = 17_280;
pub const TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
pub const TTL_EXTEND_TO: u32 = 60 * DAY_IN_LEDGERS;

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Engine,
    /// Next id to process.
    QueueHead,
    /// Next id to assign.
    QueueTail,
    /// Shares in custody for pending requests.
    PendingShares,
    /// USDC owed to processed requests.
    ReservedAssets,
    Request(u64),
    Claimable(Address),
}

fn bump_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn get_i128(e: &Env, key: &DataKey) -> i128 {
    e.storage().instance().get(key).unwrap_or(0)
}

fn set_i128(e: &Env, key: &DataKey, v: i128) {
    e.storage().instance().set(key, &v);
    bump_instance(e);
}

pub fn engine(e: &Env) -> Option<Address> {
    e.storage().instance().get(&DataKey::Engine)
}

pub fn set_engine(e: &Env, engine: &Address) {
    e.storage().instance().set(&DataKey::Engine, engine);
    bump_instance(e);
}

pub fn pending_shares(e: &Env) -> i128 {
    get_i128(e, &DataKey::PendingShares)
}

pub fn set_pending_shares(e: &Env, v: i128) {
    set_i128(e, &DataKey::PendingShares, v)
}

pub fn reserved_assets(e: &Env) -> i128 {
    get_i128(e, &DataKey::ReservedAssets)
}

pub fn set_reserved_assets(e: &Env, v: i128) {
    set_i128(e, &DataKey::ReservedAssets, v)
}

pub fn queue_head(e: &Env) -> u64 {
    e.storage().instance().get(&DataKey::QueueHead).unwrap_or(0)
}

pub fn queue_tail(e: &Env) -> u64 {
    e.storage().instance().get(&DataKey::QueueTail).unwrap_or(0)
}

pub fn set_queue_head(e: &Env, v: u64) {
    e.storage().instance().set(&DataKey::QueueHead, &v);
    bump_instance(e);
}

pub fn set_queue_tail(e: &Env, v: u64) {
    e.storage().instance().set(&DataKey::QueueTail, &v);
    bump_instance(e);
}

pub fn request(e: &Env, id: u64) -> Option<RedeemRequest> {
    e.storage().persistent().get(&DataKey::Request(id))
}

pub fn set_request(e: &Env, id: u64, r: &RedeemRequest) {
    let key = DataKey::Request(id);
    e.storage().persistent().set(&key, r);
    e.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

pub fn remove_request(e: &Env, id: u64) {
    e.storage().persistent().remove(&DataKey::Request(id));
}

pub fn claimable(e: &Env, owner: &Address) -> i128 {
    e.storage()
        .persistent()
        .get(&DataKey::Claimable(owner.clone()))
        .unwrap_or(0)
}

pub fn set_claimable(e: &Env, owner: &Address, v: i128) {
    let key = DataKey::Claimable(owner.clone());
    if v == 0 {
        e.storage().persistent().remove(&key);
    } else {
        e.storage().persistent().set(&key, &v);
        e.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    }
}
