#![no_std]
//! # agUSD: the Agama SEP-56 vault
//!
//! Users deposit USDC and receive agUSD, the vault share. agUSD is the only
//! token in the system: it is yield-bearing through its share price, there is
//! no separate 1:1 dollar token and no staking step.
//!
//! The SEP-56 surface (`deposit`, `mint`, `withdraw`, `redeem`, the `max_*`,
//! `preview_*` and `convert_*` views) and the SEP-41 token surface come from
//! the OpenZeppelin Stellar Contracts library (`stellar-tokens::vault`,
//! `stellar-tokens::fungible`). Ownership and pausing come from
//! `stellar-access::ownable` and `stellar-contract-utils::pausable`.
//!
//! Agama only adds what a private credit vault needs on top of the standard,
//! in [`accounting`] and [`queue`]:
//!
//! 1. `total_assets` counts capital deployed through the Allocation Engine,
//!    not only the USDC sitting on the vault.
//! 2. Synchronous exits are bounded by the USDC actually on hand, and are
//!    closed while a redemption queue exists, so nobody can jump the queue.
//! 3. When liquidity is short, holders queue their shares with
//!    `request_redeem` (ERC-7540 style). Shares keep earning while queued, are
//!    priced when processed, and the USDC is then claimable.
//!
//! ## Invariants
//!
//! - `total_assets = liquid_assets + engine.total_value()`
//! - `liquid_assets = USDC balance - reserved_assets`
//! - `reserved_assets` = USDC owed to processed redemptions, never deployed
//! - no capital is released to the engine while redemptions are pending

mod accounting;
mod queue;
mod storage;
#[cfg(test)]
mod test;

use agama_interfaces::VaultLiquidity;
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, token, Address, Env, MuxedAddress, String,
};
use stellar_access::ownable::{self, Ownable};
use stellar_contract_utils::pausable::{self, Pausable};
use stellar_macros::{only_owner, when_not_paused};
use stellar_tokens::{
    fungible::{total_supply::FungibleTotalSupply, Base, FungibleToken},
    vault::{FungibleVault, Vault},
};

pub use accounting::AgamaVault;
pub use queue::RedeemRequest;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum VaultError {
    InvalidAmount = 3000,
    EngineNotSet = 3001,
    InsufficientLiquidity = 3002,
    RedemptionsPending = 3003,
    RequestNotFound = 3004,
    NothingToClaim = 3005,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineSet {
    pub engine: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Released {
    #[topic]
    pub to: Address,
    pub amount: i128,
}

#[contract]
pub struct AgamaVaultContract;

#[contractimpl]
impl AgamaVaultContract {
    /// `asset` is the vault asset (USDC). agUSD has the same decimals as the
    /// asset (decimals offset 0); the OpenZeppelin virtual share/asset offset
    /// still applies, and deployment seeds the vault with a first deposit.
    pub fn __constructor(e: &Env, owner: Address, asset: Address, name: String, symbol: String) {
        Vault::set_asset(e, asset);
        Vault::set_decimals_offset(e, 0);
        Base::set_metadata(e, Vault::decimals(e), name, symbol);
        ownable::set_owner(e, &owner);
    }

    // ------------------------------------------------------------ admin

    /// Sets the Allocation Engine. It is the only address allowed to call
    /// `release`, and its `total_value()` is part of `total_assets`.
    #[only_owner]
    pub fn set_engine(e: &Env, engine: Address) {
        storage::set_engine(e, &engine);
        EngineSet { engine }.publish(e);
    }

    pub fn engine(e: &Env) -> Option<Address> {
        storage::engine(e)
    }

    // ------------------------------------------------------------ queue

    /// Queues `shares` of `owner` for redemption. The shares move into the
    /// vault's custody and keep accruing until processed. Returns the id.
    #[when_not_paused]
    pub fn request_redeem(e: &Env, shares: i128, owner: Address) -> u64 {
        owner.require_auth();
        queue::request(e, &owner, shares)
    }

    /// Cancels a request that has not been processed; the shares go back.
    pub fn cancel_redeem(e: &Env, id: u64) {
        queue::cancel(e, id)
    }

    /// Processes up to `max` requests from the head of the queue, in order,
    /// while liquidity allows. Permissionless. Returns how many were processed.
    #[when_not_paused]
    pub fn process_redeems(e: &Env, max: u32) -> u32 {
        queue::process(e, max)
    }

    /// Pays `owner` everything processed for them. Works while paused: these
    /// assets already belong to the holder.
    pub fn claim(e: &Env, owner: Address) -> i128 {
        owner.require_auth();
        queue::claim(e, &owner)
    }

    pub fn redeem_request(e: &Env, id: u64) -> Option<RedeemRequest> {
        queue::get(e, id)
    }

    /// `(head, tail)`: ids in `head..tail` may still be pending.
    pub fn queue_bounds(e: &Env) -> (u64, u64) {
        queue::bounds(e)
    }

    pub fn pending_shares(e: &Env) -> i128 {
        storage::pending_shares(e)
    }

    pub fn reserved_assets(e: &Env) -> i128 {
        storage::reserved_assets(e)
    }

    pub fn claimable(e: &Env, owner: Address) -> i128 {
        storage::claimable(e, &owner)
    }
}

// ---------------------------------------------------------------- engine side

#[contractimpl]
impl VaultLiquidity for AgamaVaultContract {
    fn liquid_assets(e: Env) -> i128 {
        accounting::liquid_assets(&e)
    }

    fn release(e: Env, amount: i128, to: Address) {
        pausable::when_not_paused(&e);
        let engine = storage::engine(&e)
            .unwrap_or_else(|| soroban_sdk::panic_with_error!(&e, VaultError::EngineNotSet));
        engine.require_auth();
        if amount <= 0 {
            soroban_sdk::panic_with_error!(&e, VaultError::InvalidAmount);
        }
        if storage::pending_shares(&e) > 0 {
            soroban_sdk::panic_with_error!(&e, VaultError::RedemptionsPending);
        }
        if amount > accounting::liquid_assets(&e) {
            soroban_sdk::panic_with_error!(&e, VaultError::InsufficientLiquidity);
        }
        token::Client::new(&e, &Vault::query_asset(&e)).transfer(
            &e.current_contract_address(),
            &to,
            &amount,
        );
        Released { to, amount }.publish(&e);
    }
}

// ---------------------------------------------------------------- standards

#[contractimpl(contracttrait)]
impl FungibleToken for AgamaVaultContract {
    type ContractType = AgamaVault;
}

#[contractimpl(contracttrait)]
impl FungibleTotalSupply for AgamaVaultContract {}

#[contractimpl(contracttrait)]
impl FungibleVault for AgamaVaultContract {}

#[contractimpl(contracttrait)]
impl Ownable for AgamaVaultContract {}

#[contractimpl(contracttrait)]
impl Pausable for AgamaVaultContract {
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
