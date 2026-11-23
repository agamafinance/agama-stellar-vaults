#![no_std]
//! # Private credit adapter (Tenka tkUSD)
//!
//! Holds the vault's position in a private credit fund that settles outside
//! Stellar. The first instance targets Tenka: its tkUSD is an ERC-4626 fund
//! share on Ethereum, subscribed and redeemed through Tenka's KYC'd Matching
//! Engine, with a weekly fund NAV published by the Tenka Oracle.
//!
//! The position moves through three on-chain buckets, each changed atomically
//! with the USDC movement it describes, so nothing is ever counted twice:
//!
//! ```text
//!  idle USDC ──dispatch──▶ in transit ──confirm_subscription──▶ units
//!      ▲                       │                                  │
//!      └──────cancel_transit───┘          settle_redemption ◀─────┘
//!      └────────────────────────────────────────┘ (USDC back, units burned)
//! ```
//!
//! - `dispatch` (operator): sends idle USDC to the operator, who bridges it
//!   (Circle CCTP) and subscribes tkUSD on Ethereum.
//! - `confirm_subscription` (operator): books the units received.
//! - `settle_redemption` (operator): pays the USDC back into the adapter and
//!   removes the units redeemed, in the same call.
//!
//! Valuation: `idle + in_transit + units × price`, where the price is the
//! tkUSD share price read from a SEP-40 oracle, refused if older than
//! `max_age`. A stale price makes `total_value` panic, which stops vault
//! entries and exits instead of letting them happen at a wrong price.
//!
//! Trust: the operator reports unit counts it controls off-chain. Every
//! subscription and redemption is checked against the oracle price within
//! `max_slippage_bps`, so a misreport is bounded, and every report is an
//! event that can be reconciled against Ethereum.

#[cfg(test)]
mod test;

use agama_interfaces::{value_of, Asset, Sep40OracleClient, StrategyAdapter, BPS};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, token,
    Address, Env,
};
use stellar_access::ownable::{self, Ownable};
use stellar_macros::only_owner;

const DAY_IN_LEDGERS: u32 = 17_280;
const TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
const TTL_EXTEND_TO: u32 = 60 * DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AdapterError {
    InvalidAmount = 6000,
    InsufficientIdle = 6001,
    ExceedsInTransit = 6002,
    ExceedsUnits = 6003,
    StalePrice = 6004,
    NoPrice = 6005,
    SlippageTooHigh = 6006,
    Overflow = 6007,
    InvalidConfig = 6008,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub engine: Address,
    pub asset: Address,
    pub operator: Address,
    pub oracle: Address,
    /// SEP-40 feed of one unit's price, e.g. `Other("TKUSD")`.
    pub feed: Asset,
    /// Decimals of the units (tkUSD: 6).
    pub unit_decimals: u32,
    /// Oldest acceptable price, in seconds. Tenka publishes weekly and
    /// treats a NAV as stale after one missed week, so 14 days.
    pub max_age: u64,
    /// Tolerance between operator-reported fills and the oracle price.
    pub max_slippage_bps: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    InTransit,
    Units,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dispatched {
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitCancelled {
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionConfirmed {
    pub usdc: i128,
    pub units: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedemptionSettled {
    pub units: i128,
    pub usdc: i128,
}

#[contract]
pub struct PrivateCreditAdapter;

fn bump(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn config(e: &Env) -> Config {
    e.storage().instance().get(&DataKey::Config).unwrap()
}

fn get(e: &Env, k: &DataKey) -> i128 {
    e.storage().instance().get(k).unwrap_or(0)
}

fn put(e: &Env, k: &DataKey, v: i128) {
    e.storage().instance().set(k, &v);
    bump(e);
}

fn idle(e: &Env, c: &Config) -> i128 {
    token::Client::new(e, &c.asset).balance(&e.current_contract_address())
}

fn validate(e: &Env, c: &Config) {
    if c.max_age == 0 || c.max_slippage_bps as i128 > BPS {
        panic_with_error!(e, AdapterError::InvalidConfig);
    }
}

/// Value of `units` at the oracle price, refusing a missing or stale price.
fn units_value(e: &Env, c: &Config, units: i128) -> i128 {
    if units == 0 {
        return 0;
    }
    let oracle = Sep40OracleClient::new(e, &c.oracle);
    let p = oracle
        .lastprice(&c.feed)
        .unwrap_or_else(|| panic_with_error!(e, AdapterError::NoPrice));
    if e.ledger().timestamp().saturating_sub(p.timestamp) > c.max_age {
        panic_with_error!(e, AdapterError::StalePrice);
    }
    let asset_decimals = token::Client::new(e, &c.asset).decimals();
    value_of(
        units,
        p.price,
        c.unit_decimals,
        oracle.decimals(),
        asset_decimals,
    )
    .unwrap_or_else(|| panic_with_error!(e, AdapterError::Overflow))
}

/// `usdc` must be within `max_slippage_bps` of the oracle value of `units`.
fn check_fill(e: &Env, c: &Config, usdc: i128, units: i128) {
    let fair = units_value(e, c, units);
    if (usdc - fair).abs() * BPS > fair * c.max_slippage_bps as i128 {
        panic_with_error!(e, AdapterError::SlippageTooHigh);
    }
}

fn positive(e: &Env, amount: i128) {
    if amount <= 0 {
        panic_with_error!(e, AdapterError::InvalidAmount);
    }
}

#[contractimpl]
impl PrivateCreditAdapter {
    pub fn __constructor(e: &Env, owner: Address, config: Config) {
        validate(e, &config);
        ownable::set_owner(e, &owner);
        e.storage().instance().set(&DataKey::Config, &config);
        bump(e);
    }

    // ------------------------------------------------------------ owner

    #[only_owner]
    pub fn set_operator(e: &Env, operator: Address) {
        let mut c = config(e);
        c.operator = operator;
        e.storage().instance().set(&DataKey::Config, &c);
    }

    #[only_owner]
    pub fn set_limits(e: &Env, max_age: u64, max_slippage_bps: u32) {
        let mut c = config(e);
        c.max_age = max_age;
        c.max_slippage_bps = max_slippage_bps;
        validate(e, &c);
        e.storage().instance().set(&DataKey::Config, &c);
    }

    // ------------------------------------------------------------ operator

    /// Sends idle USDC to the operator for bridging and subscription.
    pub fn dispatch(e: &Env, amount: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, amount);
        if amount > idle(e, &c) {
            panic_with_error!(e, AdapterError::InsufficientIdle);
        }
        token::Client::new(e, &c.asset).transfer(
            &e.current_contract_address(),
            &c.operator,
            &amount,
        );
        put(e, &DataKey::InTransit, get(e, &DataKey::InTransit) + amount);
        Dispatched { amount }.publish(e);
    }

    /// Returns USDC that could not be invested back to the adapter.
    pub fn cancel_transit(e: &Env, amount: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, amount);
        let in_transit = get(e, &DataKey::InTransit);
        if amount > in_transit {
            panic_with_error!(e, AdapterError::ExceedsInTransit);
        }
        token::Client::new(e, &c.asset).transfer(
            &c.operator,
            e.current_contract_address(),
            &amount,
        );
        put(e, &DataKey::InTransit, in_transit - amount);
        TransitCancelled { amount }.publish(e);
    }

    /// Books `units` received for `usdc` of the in-transit amount.
    pub fn confirm_subscription(e: &Env, usdc: i128, units: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, usdc);
        positive(e, units);
        let in_transit = get(e, &DataKey::InTransit);
        if usdc > in_transit {
            panic_with_error!(e, AdapterError::ExceedsInTransit);
        }
        check_fill(e, &c, usdc, units);
        put(e, &DataKey::InTransit, in_transit - usdc);
        put(e, &DataKey::Units, get(e, &DataKey::Units) + units);
        SubscriptionConfirmed { usdc, units }.publish(e);
    }

    /// Pays `usdc` from the operator into the adapter and removes the `units`
    /// it redeemed, atomically.
    pub fn settle_redemption(e: &Env, units: i128, usdc: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, units);
        positive(e, usdc);
        let held = get(e, &DataKey::Units);
        if units > held {
            panic_with_error!(e, AdapterError::ExceedsUnits);
        }
        check_fill(e, &c, usdc, units);
        token::Client::new(e, &c.asset).transfer(&c.operator, e.current_contract_address(), &usdc);
        put(e, &DataKey::Units, held - units);
        RedemptionSettled { units, usdc }.publish(e);
    }

    // ------------------------------------------------------------ views

    pub fn config(e: &Env) -> Config {
        config(e)
    }

    pub fn in_transit(e: &Env) -> i128 {
        get(e, &DataKey::InTransit)
    }

    pub fn units(e: &Env) -> i128 {
        get(e, &DataKey::Units)
    }
}

#[contractimpl]
impl StrategyAdapter for PrivateCreditAdapter {
    fn asset(e: Env) -> Address {
        config(&e).asset
    }

    fn total_value(e: Env) -> i128 {
        let c = config(&e);
        idle(&e, &c) + get(&e, &DataKey::InTransit) + units_value(&e, &c, get(&e, &DataKey::Units))
    }

    fn available(e: Env) -> i128 {
        idle(&e, &config(&e))
    }

    /// USDC is already on the adapter (sent by the vault); it stays idle
    /// until the operator dispatches it.
    fn deposit(e: Env, amount: i128) {
        config(&e).engine.require_auth();
        positive(&e, amount);
    }

    fn withdraw(e: Env, amount: i128, to: Address) -> i128 {
        let c = config(&e);
        c.engine.require_auth();
        positive(&e, amount);
        if amount > idle(&e, &c) {
            panic_with_error!(&e, AdapterError::InsufficientIdle);
        }
        token::Client::new(&e, &c.asset).transfer(&e.current_contract_address(), &to, &amount);
        amount
    }
}

#[contractimpl(contracttrait)]
impl Ownable for PrivateCreditAdapter {}
