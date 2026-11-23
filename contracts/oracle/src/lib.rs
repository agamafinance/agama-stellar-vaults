#![no_std]
//! # Agama oracle: a SEP-40 push oracle
//!
//! Publishes the prices the adapters need to value their positions, behind
//! the standard SEP-40 read interface (the one Reflector implements), so an
//! adapter can read Reflector or this contract through the same client.
//!
//! Feeds are keyed by SEP-40 [`Asset`]. Typical feeds:
//!
//! - `Other("TKUSD")`: the Tenka tkUSD share price, relayed from the Tenka
//!   Oracle (weekly fund NAV) on Ethereum.
//! - `Stellar(<stablebond>)`: an Etherfuse Stablebond price.
//!
//! Writes:
//!
//! - Reporters (role `reporter`, granted by the owner) push prices.
//!   A push that moves the price by more than the feed's `max_deviation_bps`
//!   is refused; a bigger move has to be pushed by the owner with
//!   `force_price`, which emits a distinct event.
//! - Timestamps must strictly increase and cannot be in the future.
//!
//! Freshness is the consumer's decision: every price carries its timestamp
//! and each adapter enforces its own maximum age.

#[cfg(test)]
mod test;

use agama_interfaces::{Asset, PriceData, Sep40Oracle, BPS};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, Address,
    Env, Vec,
};
use stellar_access::ownable::{self, Ownable};
use stellar_macros::only_owner;

const DAY_IN_LEDGERS: u32 = 17_280;
const TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
const TTL_EXTEND_TO: u32 = 60 * DAY_IN_LEDGERS;
/// Records kept per feed for `price` / `prices`.
pub const HISTORY: u32 = 32;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleError {
    UnknownFeed = 4000,
    NotReporter = 4001,
    InvalidPrice = 4002,
    StaleTimestamp = 4003,
    FutureTimestamp = 4004,
    DeviationTooLarge = 4005,
    FeedExists = 4006,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedConfig {
    /// Largest move a reporter can push, in basis points of the last price.
    /// `0` disables the bound (deterministic feeds).
    pub max_deviation_bps: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Base,
    Decimals,
    Resolution,
    Assets,
    Reporter(Address),
    Feed(Asset),
    /// Ring buffer of the last `HISTORY` records: (asset, slot).
    Record(Asset, u32),
    /// Number of records ever written for the asset.
    Count(Asset),
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceUpdated {
    #[topic]
    pub asset: Asset,
    pub price: i128,
    pub timestamp: u64,
    pub forced: bool,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReporterSet {
    #[topic]
    pub reporter: Address,
    pub allowed: bool,
}

#[contract]
pub struct AgamaOracle;

fn bump(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn feed(e: &Env, asset: &Asset) -> FeedConfig {
    e.storage()
        .persistent()
        .get(&DataKey::Feed(asset.clone()))
        .unwrap_or_else(|| panic_with_error!(e, OracleError::UnknownFeed))
}

fn count(e: &Env, asset: &Asset) -> u32 {
    e.storage()
        .persistent()
        .get(&DataKey::Count(asset.clone()))
        .unwrap_or(0)
}

fn record(e: &Env, asset: &Asset, index: u32) -> Option<PriceData> {
    e.storage()
        .persistent()
        .get(&DataKey::Record(asset.clone(), index % HISTORY))
}

fn last(e: &Env, asset: &Asset) -> Option<PriceData> {
    let n = count(e, asset);
    if n == 0 {
        None
    } else {
        record(e, asset, n - 1)
    }
}

fn write(e: &Env, asset: &Asset, price: i128, timestamp: u64, forced: bool) {
    if price <= 0 {
        panic_with_error!(e, OracleError::InvalidPrice);
    }
    if timestamp > e.ledger().timestamp() {
        panic_with_error!(e, OracleError::FutureTimestamp);
    }
    let cfg = feed(e, asset);
    if let Some(prev) = last(e, asset) {
        if timestamp <= prev.timestamp {
            panic_with_error!(e, OracleError::StaleTimestamp);
        }
        if !forced && cfg.max_deviation_bps > 0 {
            let diff = (price - prev.price).abs();
            if diff * BPS > prev.price * cfg.max_deviation_bps as i128 {
                panic_with_error!(e, OracleError::DeviationTooLarge);
            }
        }
    }
    let n = count(e, asset);
    let rec_key = DataKey::Record(asset.clone(), n % HISTORY);
    let count_key = DataKey::Count(asset.clone());
    let p = e.storage().persistent();
    p.set(&rec_key, &PriceData { price, timestamp });
    p.set(&count_key, &(n + 1));
    p.extend_ttl(&rec_key, TTL_THRESHOLD, TTL_EXTEND_TO);
    p.extend_ttl(&count_key, TTL_THRESHOLD, TTL_EXTEND_TO);
    p.extend_ttl(&DataKey::Feed(asset.clone()), TTL_THRESHOLD, TTL_EXTEND_TO);
    bump(e);
    PriceUpdated {
        asset: asset.clone(),
        price,
        timestamp,
        forced,
    }
    .publish(e);
}

#[contractimpl]
impl AgamaOracle {
    /// `base` is the quote asset (USDC); `decimals` the price precision;
    /// `resolution` the expected update period in seconds (informational).
    pub fn __constructor(e: &Env, owner: Address, base: Asset, decimals: u32, resolution: u32) {
        ownable::set_owner(e, &owner);
        let i = e.storage().instance();
        i.set(&DataKey::Base, &base);
        i.set(&DataKey::Decimals, &decimals);
        i.set(&DataKey::Resolution, &resolution);
        i.set(&DataKey::Assets, &Vec::<Asset>::new(e));
    }

    #[only_owner]
    pub fn add_feed(e: &Env, asset: Asset, max_deviation_bps: u32) {
        let key = DataKey::Feed(asset.clone());
        if e.storage().persistent().has(&key) {
            panic_with_error!(e, OracleError::FeedExists);
        }
        e.storage()
            .persistent()
            .set(&key, &FeedConfig { max_deviation_bps });
        e.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
        let mut assets: Vec<Asset> = e.storage().instance().get(&DataKey::Assets).unwrap();
        assets.push_back(asset);
        e.storage().instance().set(&DataKey::Assets, &assets);
        bump(e);
    }

    #[only_owner]
    pub fn set_max_deviation(e: &Env, asset: Asset, max_deviation_bps: u32) {
        feed(e, &asset);
        let key = DataKey::Feed(asset);
        e.storage()
            .persistent()
            .set(&key, &FeedConfig { max_deviation_bps });
    }

    pub fn feed_config(e: &Env, asset: Asset) -> FeedConfig {
        feed(e, &asset)
    }

    #[only_owner]
    pub fn set_reporter(e: &Env, reporter: Address, allowed: bool) {
        let key = DataKey::Reporter(reporter.clone());
        if allowed {
            e.storage().instance().set(&key, &true);
        } else {
            e.storage().instance().remove(&key);
        }
        bump(e);
        ReporterSet { reporter, allowed }.publish(e);
    }

    pub fn is_reporter(e: &Env, reporter: Address) -> bool {
        e.storage().instance().has(&DataKey::Reporter(reporter))
    }

    /// Pushes a price, bounded by the feed's deviation limit.
    pub fn set_price(e: &Env, reporter: Address, asset: Asset, price: i128, timestamp: u64) {
        reporter.require_auth();
        if !e.storage().instance().has(&DataKey::Reporter(reporter)) {
            panic_with_error!(e, OracleError::NotReporter);
        }
        write(e, &asset, price, timestamp, false);
    }

    /// Owner override for a move beyond the deviation bound (write-down,
    /// recovery). Emits `PriceUpdated` with `forced = true`.
    #[only_owner]
    pub fn force_price(e: &Env, asset: Asset, price: i128, timestamp: u64) {
        write(e, &asset, price, timestamp, true);
    }
}

#[contractimpl]
impl Sep40Oracle for AgamaOracle {
    fn base(e: Env) -> Asset {
        e.storage().instance().get(&DataKey::Base).unwrap()
    }

    fn assets(e: Env) -> Vec<Asset> {
        e.storage().instance().get(&DataKey::Assets).unwrap()
    }

    fn decimals(e: Env) -> u32 {
        e.storage().instance().get(&DataKey::Decimals).unwrap()
    }

    fn resolution(e: Env) -> u32 {
        e.storage().instance().get(&DataKey::Resolution).unwrap()
    }

    /// The most recent record at or before `timestamp`, within history.
    fn price(e: Env, asset: Asset, timestamp: u64) -> Option<PriceData> {
        let n = count(&e, &asset);
        let oldest = n.saturating_sub(HISTORY);
        let mut i = n;
        while i > oldest {
            i -= 1;
            if let Some(r) = record(&e, &asset, i) {
                if r.timestamp <= timestamp {
                    return Some(r);
                }
            }
        }
        None
    }

    /// Up to `records` most recent records, newest first.
    fn prices(e: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>> {
        let n = count(&e, &asset);
        if n == 0 {
            return None;
        }
        let take = records.min(n).min(HISTORY);
        let mut out = Vec::new(&e);
        for k in 0..take {
            if let Some(r) = record(&e, &asset, n - 1 - k) {
                out.push_back(r);
            }
        }
        Some(out)
    }

    fn lastprice(e: Env, asset: Asset) -> Option<PriceData> {
        last(&e, &asset)
    }
}

#[contractimpl(contracttrait)]
impl Ownable for AgamaOracle {}
