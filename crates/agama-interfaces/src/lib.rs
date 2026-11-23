#![no_std]
//! Interfaces shared by the Agama contracts.
//!
//! Nothing in this crate holds state. It declares, once, the shape of every
//! cross-contract call in the system so that each contract depends on an
//! interface rather than on another contract's crate:
//!
//! - [`Sep40Oracle`]: the SEP-40 price feed interface (the one Reflector
//!   implements). The Agama oracle implements it and the adapters consume it.
//! - [`StrategyAdapter`]: the single interface every pool adapter exposes to
//!   the Allocation Engine.
//! - [`AllocationEngineView`]: what the vault reads from the engine.
//! - [`VaultLiquidity`]: what the engine calls on the vault.

use soroban_sdk::{contractclient, contracttype, Address, Env, Symbol, Vec};

// ---------------------------------------------------------------- SEP-40

/// SEP-40 asset identifier.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Asset {
    Stellar(Address),
    Other(Symbol),
}

/// SEP-40 price record.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

/// SEP-40 oracle interface (read side).
#[contractclient(name = "Sep40OracleClient")]
pub trait Sep40Oracle {
    fn base(e: Env) -> Asset;
    fn assets(e: Env) -> Vec<Asset>;
    fn decimals(e: Env) -> u32;
    fn resolution(e: Env) -> u32;
    fn price(e: Env, asset: Asset, timestamp: u64) -> Option<PriceData>;
    fn prices(e: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>>;
    fn lastprice(e: Env, asset: Asset) -> Option<PriceData>;
}

// ---------------------------------------------------------------- Adapters

/// The interface every pool adapter exposes. Amounts are in units of the
/// vault asset (USDC).
///
/// `deposit` and `withdraw` are restricted to the Allocation Engine.
#[contractclient(name = "StrategyAdapterClient")]
pub trait StrategyAdapter {
    /// The asset the adapter receives and returns (the vault asset).
    fn asset(e: Env) -> Address;
    /// Current value of everything the adapter holds, in vault-asset units.
    /// Panics if the valuation is stale: an unpriceable position must stop
    /// the vault, not be valued at an arbitrary number.
    fn total_value(e: Env) -> i128;
    /// Value that can be returned to the vault right now.
    fn available(e: Env) -> i128;
    /// Called by the engine after it transferred `amount` of the asset to the
    /// adapter.
    fn deposit(e: Env, amount: i128);
    /// Sends `amount` of the asset to `to` (always the vault). Returns the
    /// amount sent.
    fn withdraw(e: Env, amount: i128, to: Address) -> i128;
}

// ---------------------------------------------------------------- Engine / Vault

/// Read side of the Allocation Engine, used by the vault's `total_assets`.
#[contractclient(name = "AllocationEngineViewClient")]
pub trait AllocationEngineView {
    /// Sum of `total_value()` over every registered adapter.
    fn total_value(e: Env) -> i128;
}

/// What the Allocation Engine is allowed to call on the vault.
#[contractclient(name = "VaultLiquidityClient")]
pub trait VaultLiquidity {
    /// Vault asset held by the vault and not reserved for processed
    /// redemptions.
    fn liquid_assets(e: Env) -> i128;
    /// Moves `amount` of liquid assets to `to`. Engine only.
    fn release(e: Env, amount: i128, to: Address);
}

/// Basis points denominator.
pub const BPS: i128 = 10_000;
