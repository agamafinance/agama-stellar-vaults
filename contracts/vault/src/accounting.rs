//! The vault contract type: OpenZeppelin's `Vault` with Agama's accounting.
//!
//! [`AgamaVault`] implements `VaultOverrides`, the hook the OpenZeppelin
//! `FungibleVault` trait routes every SEP-56 call through. The conversion
//! formula is OpenZeppelin's, unchanged (virtual offset, explicit rounding);
//! it is restated here only because the library version reads
//! `total_assets` as the vault's own balance, and Agama's assets include the
//! capital deployed through the Allocation Engine.

use agama_interfaces::AllocationEngineViewClient;
use soroban_sdk::{panic_with_error, token, Address, Env};
use stellar_contract_utils::{
    math::{i128_fixed_point::mul_div_with_rounding, Rounding},
    pausable,
};
use stellar_tokens::{
    fungible::{
        total_supply::{total_supply, TotalSupplyOverrides},
        ContractOverrides,
    },
    vault::{emit_deposit, emit_withdraw, Vault, VaultOverrides, VaultTokenError},
};

use crate::storage;

pub struct AgamaVault;

impl ContractOverrides for AgamaVault {
    fn decimals(e: &Env) -> u32 {
        Vault::decimals(e)
    }
}

impl TotalSupplyOverrides for AgamaVault {}

/// USDC on the vault that is not owed to processed redemptions.
pub fn liquid_assets(e: &Env) -> i128 {
    let balance =
        token::Client::new(e, &Vault::query_asset(e)).balance(&e.current_contract_address());
    balance - storage::reserved_assets(e)
}

/// Value of the capital deployed through the Allocation Engine.
pub fn deployed_assets(e: &Env) -> i128 {
    match storage::engine(e) {
        Some(engine) => AllocationEngineViewClient::new(e, &engine).total_value(),
        None => 0,
    }
}

fn to_shares(e: &Env, assets: i128, rounding: Rounding) -> i128 {
    if assets < 0 {
        panic_with_error!(e, VaultTokenError::VaultInvalidAssetsAmount);
    }
    if assets == 0 {
        return 0;
    }
    let supply = total_supply(e) + 1; // virtual offset 10^0
    let assets_total = AgamaVault::total_assets(e) + 1;
    mul_div_with_rounding(e, assets, supply, assets_total, rounding)
}

fn to_assets(e: &Env, shares: i128, rounding: Rounding) -> i128 {
    if shares < 0 {
        panic_with_error!(e, VaultTokenError::VaultInvalidSharesAmount);
    }
    if shares == 0 {
        return 0;
    }
    let supply = total_supply(e) + 1;
    let assets_total = AgamaVault::total_assets(e) + 1;
    mul_div_with_rounding(e, shares, assets_total, supply, rounding)
}

/// Shares a holder can exit synchronously: zero while paused or while a
/// redemption queue exists (exits are first come, first served).
fn exit_open(e: &Env) -> bool {
    !pausable::paused(e) && storage::pending_shares(e) == 0
}

impl AgamaVault {
    pub fn convert_to_assets_floor(e: &Env, shares: i128) -> i128 {
        to_assets(e, shares, Rounding::Floor)
    }
}

impl VaultOverrides for AgamaVault {
    fn total_assets(e: &Env) -> i128 {
        liquid_assets(e) + deployed_assets(e)
    }

    fn convert_to_shares(e: &Env, assets: i128) -> i128 {
        to_shares(e, assets, Rounding::Floor)
    }

    fn convert_to_assets(e: &Env, shares: i128) -> i128 {
        to_assets(e, shares, Rounding::Floor)
    }

    fn max_deposit(e: &Env, _receiver: Address) -> i128 {
        if pausable::paused(e) {
            0
        } else {
            i128::MAX
        }
    }

    fn preview_deposit(e: &Env, assets: i128) -> i128 {
        to_shares(e, assets, Rounding::Floor)
    }

    fn deposit(e: &Env, assets: i128, receiver: Address, from: Address, operator: Address) -> i128 {
        operator.require_auth();
        pausable::when_not_paused(e);
        if assets <= 0 {
            panic_with_error!(e, VaultTokenError::VaultInvalidAssetsAmount);
        }
        let shares = to_shares(e, assets, Rounding::Floor);
        if shares == 0 {
            panic_with_error!(e, VaultTokenError::VaultZeroShares);
        }
        Vault::deposit_internal(e, &receiver, assets, shares, &from, &operator);
        emit_deposit(e, &operator, &from, &receiver, assets, shares);
        shares
    }

    fn max_mint(e: &Env, _receiver: Address) -> i128 {
        if pausable::paused(e) {
            0
        } else {
            i128::MAX
        }
    }

    fn preview_mint(e: &Env, shares: i128) -> i128 {
        to_assets(e, shares, Rounding::Ceil)
    }

    fn mint(e: &Env, shares: i128, receiver: Address, from: Address, operator: Address) -> i128 {
        operator.require_auth();
        pausable::when_not_paused(e);
        if shares <= 0 {
            panic_with_error!(e, VaultTokenError::VaultInvalidSharesAmount);
        }
        let assets = to_assets(e, shares, Rounding::Ceil);
        Vault::deposit_internal(e, &receiver, assets, shares, &from, &operator);
        emit_deposit(e, &operator, &from, &receiver, assets, shares);
        assets
    }

    fn max_withdraw(e: &Env, owner: Address) -> i128 {
        if !exit_open(e) {
            return 0;
        }
        let owned = to_assets(e, Vault::balance(e, &owner), Rounding::Floor);
        owned.min(liquid_assets(e))
    }

    fn preview_withdraw(e: &Env, assets: i128) -> i128 {
        to_shares(e, assets, Rounding::Ceil)
    }

    fn withdraw(
        e: &Env,
        assets: i128,
        receiver: Address,
        owner: Address,
        operator: Address,
    ) -> i128 {
        operator.require_auth();
        if assets <= 0 {
            panic_with_error!(e, VaultTokenError::VaultInvalidAssetsAmount);
        }
        if assets > Self::max_withdraw(e, owner.clone()) {
            panic_with_error!(e, VaultTokenError::VaultExceededMaxWithdraw);
        }
        let shares = to_shares(e, assets, Rounding::Ceil);
        Vault::withdraw_internal(e, &receiver, &owner, assets, shares, &operator);
        emit_withdraw(e, &operator, &receiver, &owner, assets, shares);
        shares
    }

    fn max_redeem(e: &Env, owner: Address) -> i128 {
        if !exit_open(e) {
            return 0;
        }
        let balance = Vault::balance(e, &owner);
        let liquid_shares = to_shares(e, liquid_assets(e), Rounding::Floor);
        let shares = balance.min(liquid_shares);
        if to_assets(e, shares, Rounding::Floor) == 0 {
            return 0;
        }
        shares
    }

    fn preview_redeem(e: &Env, shares: i128) -> i128 {
        to_assets(e, shares, Rounding::Floor)
    }

    fn redeem(e: &Env, shares: i128, receiver: Address, owner: Address, operator: Address) -> i128 {
        operator.require_auth();
        if shares <= 0 {
            panic_with_error!(e, VaultTokenError::VaultInvalidSharesAmount);
        }
        if shares > Self::max_redeem(e, owner.clone()) {
            panic_with_error!(e, VaultTokenError::VaultExceededMaxRedeem);
        }
        let assets = to_assets(e, shares, Rounding::Floor);
        if assets == 0 {
            panic_with_error!(e, VaultTokenError::VaultZeroAssets);
        }
        Vault::withdraw_internal(e, &receiver, &owner, assets, shares, &operator);
        emit_withdraw(e, &operator, &receiver, &owner, assets, shares);
        assets
    }
}
