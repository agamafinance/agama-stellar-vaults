#![no_std]
//! # Etherfuse Stablebond adapter
//!
//! Holds Etherfuse Stablebonds (CETES, USTRY, TESOURO): tokenized government
//! bonds issued on Stellar as classic assets, each with its Stellar Asset
//! Contract. They are bought and sold through Etherfuse's API (quote, then an
//! order settled to a KYB'd wallet), not through an on-chain pool, so the
//! adapter follows the same operator-settled model as the private credit
//! adapter, with one difference: the bonds themselves sit on the adapter.
//!
//! ```text
//!  idle USDC ──dispatch──▶ usdc_in_transit ──confirm_purchase(usdc, bonds)──▶ bonds on the adapter
//!      ▲                        │                                                  │
//!      └─────cancel_transit─────┘                               dispatch_bonds ────┘
//!      └──────settle_sale(bonds, usdc)◀── bonds_in_transit ◀──────────────────────┘
//!                                               └──cancel_sale──▶ back on the adapter
//! ```
//!
//! Every step moves the tokens and the bookkeeping in the same call:
//! `confirm_purchase` pulls the bonds from the operator, `settle_sale` pulls
//! the USDC. Nothing the adapter values is self-reported except the two
//! in-transit amounts, which only ever hold what the adapter itself sent.
//!
//! Valuation: `idle USDC + usdc_in_transit + (bond balance + bonds_in_transit)
//! × price`, where the price is the bond's SEP-40 price (Etherfuse publishes
//! one on-chain, base USD), refused when older than `max_age`. Fills are
//! checked against that price within `max_slippage_bps`.

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
    InvalidAmount = 7000,
    InsufficientIdle = 7001,
    StalePrice = 7002,
    NoPrice = 7003,
    SlippageTooHigh = 7004,
    Overflow = 7005,
    InvalidConfig = 7006,
    ExceedsInTransit = 7007,
    InsufficientBonds = 7008,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub engine: Address,
    /// Vault asset (USDC).
    pub asset: Address,
    /// Stablebond token (its Stellar Asset Contract).
    pub bond: Address,
    /// Account KYB'd with Etherfuse that executes the API orders.
    pub operator: Address,
    /// SEP-40 oracle pricing `feed` (Etherfuse's aggregator or Agama's).
    pub oracle: Address,
    pub feed: Asset,
    /// Oldest acceptable price, in seconds.
    pub max_age: u64,
    pub max_slippage_bps: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    UsdcInTransit,
    BondsInTransit,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dispatched {
    pub usdc: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitCancelled {
    pub usdc: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Purchased {
    pub usdc: i128,
    pub bonds: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BondsDispatched {
    pub bonds: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaleCancelled {
    pub bonds: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sold {
    pub bonds: i128,
    pub usdc: i128,
}

#[contract]
pub struct EtherfuseAdapter;

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

fn balance(e: &Env, token_id: &Address) -> i128 {
    token::Client::new(e, token_id).balance(&e.current_contract_address())
}

fn validate(e: &Env, c: &Config) {
    if c.max_age == 0 || c.max_slippage_bps as i128 > BPS || c.bond == c.asset {
        panic_with_error!(e, AdapterError::InvalidConfig);
    }
}

fn positive(e: &Env, amount: i128) {
    if amount <= 0 {
        panic_with_error!(e, AdapterError::InvalidAmount);
    }
}

/// Value of `bonds` in vault-asset units at the oracle price.
fn bonds_value(e: &Env, c: &Config, bonds: i128) -> i128 {
    if bonds == 0 {
        return 0;
    }
    let oracle = Sep40OracleClient::new(e, &c.oracle);
    let p = oracle
        .lastprice(&c.feed)
        .unwrap_or_else(|| panic_with_error!(e, AdapterError::NoPrice));
    if e.ledger().timestamp().saturating_sub(p.timestamp) > c.max_age {
        panic_with_error!(e, AdapterError::StalePrice);
    }
    let bond_decimals = token::Client::new(e, &c.bond).decimals();
    let asset_decimals = token::Client::new(e, &c.asset).decimals();
    value_of(
        bonds,
        p.price,
        bond_decimals,
        oracle.decimals(),
        asset_decimals,
    )
    .unwrap_or_else(|| panic_with_error!(e, AdapterError::Overflow))
}

fn check_fill(e: &Env, c: &Config, usdc: i128, bonds: i128) {
    let fair = bonds_value(e, c, bonds);
    if (usdc - fair).abs() * BPS > fair * c.max_slippage_bps as i128 {
        panic_with_error!(e, AdapterError::SlippageTooHigh);
    }
}

#[contractimpl]
impl EtherfuseAdapter {
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

    // ------------------------------------------------------------ buy

    /// Sends idle USDC to the operator, who swaps it for bonds via Etherfuse.
    pub fn dispatch(e: &Env, usdc: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, usdc);
        if usdc > balance(e, &c.asset) {
            panic_with_error!(e, AdapterError::InsufficientIdle);
        }
        token::Client::new(e, &c.asset).transfer(&e.current_contract_address(), &c.operator, &usdc);
        put(
            e,
            &DataKey::UsdcInTransit,
            get(e, &DataKey::UsdcInTransit) + usdc,
        );
        Dispatched { usdc }.publish(e);
    }

    /// Returns USDC whose purchase did not happen.
    pub fn cancel_transit(e: &Env, usdc: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, usdc);
        let t = get(e, &DataKey::UsdcInTransit);
        if usdc > t {
            panic_with_error!(e, AdapterError::ExceedsInTransit);
        }
        token::Client::new(e, &c.asset).transfer(&c.operator, e.current_contract_address(), &usdc);
        put(e, &DataKey::UsdcInTransit, t - usdc);
        TransitCancelled { usdc }.publish(e);
    }

    /// Delivers the `bonds` bought with `usdc` of the in-transit USDC.
    pub fn confirm_purchase(e: &Env, usdc: i128, bonds: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, usdc);
        positive(e, bonds);
        let t = get(e, &DataKey::UsdcInTransit);
        if usdc > t {
            panic_with_error!(e, AdapterError::ExceedsInTransit);
        }
        check_fill(e, &c, usdc, bonds);
        token::Client::new(e, &c.bond).transfer(&c.operator, e.current_contract_address(), &bonds);
        put(e, &DataKey::UsdcInTransit, t - usdc);
        Purchased { usdc, bonds }.publish(e);
    }

    // ------------------------------------------------------------ sell

    /// Sends bonds to the operator for sale via Etherfuse.
    pub fn dispatch_bonds(e: &Env, bonds: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, bonds);
        if bonds > balance(e, &c.bond) {
            panic_with_error!(e, AdapterError::InsufficientBonds);
        }
        token::Client::new(e, &c.bond).transfer(&e.current_contract_address(), &c.operator, &bonds);
        put(
            e,
            &DataKey::BondsInTransit,
            get(e, &DataKey::BondsInTransit) + bonds,
        );
        BondsDispatched { bonds }.publish(e);
    }

    /// Returns bonds whose sale did not happen.
    pub fn cancel_sale(e: &Env, bonds: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, bonds);
        let t = get(e, &DataKey::BondsInTransit);
        if bonds > t {
            panic_with_error!(e, AdapterError::ExceedsInTransit);
        }
        token::Client::new(e, &c.bond).transfer(&c.operator, e.current_contract_address(), &bonds);
        put(e, &DataKey::BondsInTransit, t - bonds);
        SaleCancelled { bonds }.publish(e);
    }

    /// Delivers the `usdc` received for `bonds` of the in-transit bonds.
    pub fn settle_sale(e: &Env, bonds: i128, usdc: i128) {
        let c = config(e);
        c.operator.require_auth();
        positive(e, bonds);
        positive(e, usdc);
        let t = get(e, &DataKey::BondsInTransit);
        if bonds > t {
            panic_with_error!(e, AdapterError::ExceedsInTransit);
        }
        check_fill(e, &c, usdc, bonds);
        token::Client::new(e, &c.asset).transfer(&c.operator, e.current_contract_address(), &usdc);
        put(e, &DataKey::BondsInTransit, t - bonds);
        Sold { bonds, usdc }.publish(e);
    }

    // ------------------------------------------------------------ views

    pub fn config(e: &Env) -> Config {
        config(e)
    }

    pub fn usdc_in_transit(e: &Env) -> i128 {
        get(e, &DataKey::UsdcInTransit)
    }

    pub fn bonds_in_transit(e: &Env) -> i128 {
        get(e, &DataKey::BondsInTransit)
    }

    /// Bonds held on the adapter.
    pub fn bonds(e: &Env) -> i128 {
        balance(e, &config(e).bond)
    }
}

#[contractimpl]
impl StrategyAdapter for EtherfuseAdapter {
    fn asset(e: Env) -> Address {
        config(&e).asset
    }

    fn total_value(e: Env) -> i128 {
        let c = config(&e);
        let bonds = balance(&e, &c.bond) + get(&e, &DataKey::BondsInTransit);
        balance(&e, &c.asset) + get(&e, &DataKey::UsdcInTransit) + bonds_value(&e, &c, bonds)
    }

    fn available(e: Env) -> i128 {
        balance(&e, &config(&e).asset)
    }

    /// USDC is already on the adapter; it stays idle until dispatched.
    fn deposit(e: Env, amount: i128) {
        config(&e).engine.require_auth();
        positive(&e, amount);
    }

    fn withdraw(e: Env, amount: i128, to: Address) -> i128 {
        let c = config(&e);
        c.engine.require_auth();
        positive(&e, amount);
        if amount > balance(&e, &c.asset) {
            panic_with_error!(&e, AdapterError::InsufficientIdle);
        }
        token::Client::new(&e, &c.asset).transfer(&e.current_contract_address(), &to, &amount);
        amount
    }
}

#[contractimpl(contracttrait)]
impl Ownable for EtherfuseAdapter {}
