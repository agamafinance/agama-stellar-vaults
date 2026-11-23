#![no_std]
//! # Etherfuse Stablebond adapter
//!
//! Holds Stellar-native tokenized government bonds (Etherfuse Stablebonds):
//! the low-risk, on-chain leg of the book. Unlike private credit, the
//! position subscribes and redeems in the same transaction, so `deposit`
//! invests immediately and `withdraw` redeems what it needs.
//!
//! Valuation: `idle USDC + units × price`, the price read from a SEP-40
//! oracle (the Etherfuse feed) and refused when older than `max_age`. Every
//! subscription and redemption is checked against that price within
//! `max_slippage_bps`.
//!
//! The pool is reached through the `StablebondPool` interface in
//! `agama-interfaces`; it must be confirmed against the deployed Etherfuse
//! contracts before mainnet.

#[cfg(test)]
mod test;

use agama_interfaces::{
    value_of, Asset, Sep40OracleClient, StablebondPoolClient, StrategyAdapter, BPS,
};
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, token,
    vec, Address, Env, IntoVal, Symbol,
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
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub engine: Address,
    pub asset: Address,
    pub pool: Address,
    pub oracle: Address,
    /// SEP-40 feed of one bond unit's price.
    pub feed: Asset,
    pub unit_decimals: u32,
    pub max_age: u64,
    pub max_slippage_bps: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscribed {
    pub usdc: i128,
    pub units: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redeemed {
    pub units: i128,
    pub usdc: i128,
}

#[contract]
pub struct EtherfuseAdapter;

fn config(e: &Env) -> Config {
    e.storage().instance().get(&DataKey::Config).unwrap()
}

fn idle(e: &Env, c: &Config) -> i128 {
    token::Client::new(e, &c.asset).balance(&e.current_contract_address())
}

fn units(e: &Env, c: &Config) -> i128 {
    StablebondPoolClient::new(e, &c.pool).units(&e.current_contract_address())
}

fn validate(e: &Env, c: &Config) {
    if c.max_age == 0 || c.max_slippage_bps as i128 > BPS {
        panic_with_error!(e, AdapterError::InvalidConfig);
    }
}

/// (price, price decimals, asset decimals), refusing a missing or stale price.
fn price(e: &Env, c: &Config) -> (i128, u32, u32) {
    let oracle = Sep40OracleClient::new(e, &c.oracle);
    let p = oracle
        .lastprice(&c.feed)
        .unwrap_or_else(|| panic_with_error!(e, AdapterError::NoPrice));
    if e.ledger().timestamp().saturating_sub(p.timestamp) > c.max_age {
        panic_with_error!(e, AdapterError::StalePrice);
    }
    (
        p.price,
        oracle.decimals(),
        token::Client::new(e, &c.asset).decimals(),
    )
}

fn units_value(e: &Env, c: &Config, u: i128) -> i128 {
    if u == 0 {
        return 0;
    }
    let (p, pd, ad) = price(e, c);
    value_of(u, p, c.unit_decimals, pd, ad)
        .unwrap_or_else(|| panic_with_error!(e, AdapterError::Overflow))
}

fn check_fill(e: &Env, c: &Config, usdc: i128, u: i128) {
    let fair = units_value(e, c, u);
    if (usdc - fair).abs() * BPS > fair * c.max_slippage_bps as i128 {
        panic_with_error!(e, AdapterError::SlippageTooHigh);
    }
}

fn positive(e: &Env, amount: i128) {
    if amount <= 0 {
        panic_with_error!(e, AdapterError::InvalidAmount);
    }
}

fn subscribe(e: &Env, c: &Config, amount: i128) {
    let me = e.current_contract_address();
    // The pool pulls the USDC from this contract: authorize exactly that
    // transfer for this call.
    e.authorize_as_current_contract(vec![
        e,
        InvokerContractAuthEntry::Contract(SubContractInvocation {
            context: ContractContext {
                contract: c.asset.clone(),
                fn_name: Symbol::new(e, "transfer"),
                args: (me.clone(), c.pool.clone(), amount).into_val(e),
            },
            sub_invocations: vec![e],
        }),
    ]);
    let got = StablebondPoolClient::new(e, &c.pool).subscribe(&me, &amount);
    check_fill(e, c, amount, got);
    Subscribed {
        usdc: amount,
        units: got,
    }
    .publish(e);
}

#[contractimpl]
impl EtherfuseAdapter {
    pub fn __constructor(e: &Env, owner: Address, config: Config) {
        validate(e, &config);
        ownable::set_owner(e, &owner);
        e.storage().instance().set(&DataKey::Config, &config);
        e.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
    }

    #[only_owner]
    pub fn set_limits(e: &Env, max_age: u64, max_slippage_bps: u32) {
        let mut c = config(e);
        c.max_age = max_age;
        c.max_slippage_bps = max_slippage_bps;
        validate(e, &c);
        e.storage().instance().set(&DataKey::Config, &c);
    }

    pub fn config(e: &Env) -> Config {
        config(e)
    }

    pub fn units(e: &Env) -> i128 {
        units(e, &config(e))
    }
}

#[contractimpl]
impl StrategyAdapter for EtherfuseAdapter {
    fn asset(e: Env) -> Address {
        config(&e).asset
    }

    fn total_value(e: Env) -> i128 {
        let c = config(&e);
        idle(&e, &c) + units_value(&e, &c, units(&e, &c))
    }

    /// Everything is redeemable on-chain right away.
    fn available(e: Env) -> i128 {
        Self::total_value(e)
    }

    /// Invests the USDC the vault just sent.
    fn deposit(e: Env, amount: i128) {
        let c = config(&e);
        c.engine.require_auth();
        positive(&e, amount);
        if amount > idle(&e, &c) {
            panic_with_error!(&e, AdapterError::InsufficientIdle);
        }
        subscribe(&e, &c, amount);
        e.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
    }

    /// Redeems what is missing, then sends up to `amount` to `to`.
    fn withdraw(e: Env, amount: i128, to: Address) -> i128 {
        let c = config(&e);
        c.engine.require_auth();
        positive(&e, amount);
        let me = e.current_contract_address();
        let have = idle(&e, &c);
        if have < amount {
            let held = units(&e, &c);
            let (p, pd, ad) = price(&e, &c);
            // units for the shortfall, rounded up, capped at what is held
            let shortfall = amount - have;
            let scale_num = 10i128.pow(c.unit_decimals) * 10i128.pow(pd);
            let scale_den = p * 10i128.pow(ad);
            let need = (shortfall * scale_num + scale_den - 1) / scale_den;
            let to_redeem = need.min(held);
            if to_redeem > 0 {
                let got = StablebondPoolClient::new(&e, &c.pool).redeem(&me, &to_redeem);
                check_fill(&e, &c, got, to_redeem);
                Redeemed {
                    units: to_redeem,
                    usdc: got,
                }
                .publish(&e);
            }
        }
        let sent = amount.min(idle(&e, &c));
        token::Client::new(&e, &c.asset).transfer(&me, &to, &sent);
        sent
    }
}

#[contractimpl(contracttrait)]
impl Ownable for EtherfuseAdapter {}
