//! Asynchronous redemptions, first in first out.
//!
//! Private credit cannot be sold back in a block, so a holder whose exit
//! exceeds the USDC on hand queues their shares instead:
//!
//! 1. `request` moves the shares into the vault's custody. They stay in the
//!    supply and keep earning (or losing) like any other share.
//! 2. `process` walks the queue from the head. A request is priced at that
//!    moment, its shares are burned and its USDC is set aside
//!    (`reserved_assets`) and credited to the owner. Processing stops at the
//!    first request the liquidity cannot cover, so order is strict.
//! 3. `claim` pays the owner. Reserved USDC is outside `total_assets` and
//!    cannot be released to the engine.

use soroban_sdk::{contractevent, contracttype, panic_with_error, token, Address, Env};
use stellar_tokens::{
    fungible::{total_supply::decrease_total_supply, Base},
    vault::Vault,
};

use crate::{accounting, storage, AgamaVault, VaultError};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedeemRequest {
    pub owner: Address,
    pub shares: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedeemRequested {
    #[topic]
    pub owner: Address,
    pub id: u64,
    pub shares: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedeemCancelled {
    #[topic]
    pub owner: Address,
    pub id: u64,
    pub shares: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedeemProcessed {
    #[topic]
    pub owner: Address,
    pub id: u64,
    pub shares: i128,
    pub assets: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claimed {
    #[topic]
    pub owner: Address,
    pub assets: i128,
}

pub fn request(e: &Env, owner: &Address, shares: i128) -> u64 {
    if shares <= 0 {
        panic_with_error!(e, VaultError::InvalidAmount);
    }
    let vault = e.current_contract_address();
    Base::update(e, Some(owner), Some(&vault), shares);

    let id = storage::queue_tail(e);
    storage::set_request(
        e,
        id,
        &RedeemRequest {
            owner: owner.clone(),
            shares,
        },
    );
    storage::set_queue_tail(e, id + 1);
    storage::set_pending_shares(e, storage::pending_shares(e) + shares);
    RedeemRequested {
        owner: owner.clone(),
        id,
        shares,
    }
    .publish(e);
    id
}

pub fn cancel(e: &Env, id: u64) {
    let r = storage::request(e, id)
        .unwrap_or_else(|| panic_with_error!(e, VaultError::RequestNotFound));
    r.owner.require_auth();
    let vault = e.current_contract_address();
    Base::update(e, Some(&vault), Some(&r.owner), r.shares);
    storage::remove_request(e, id);
    storage::set_pending_shares(e, storage::pending_shares(e) - r.shares);
    RedeemCancelled {
        owner: r.owner,
        id,
        shares: r.shares,
    }
    .publish(e);
}

pub fn process(e: &Env, max: u32) -> u32 {
    let vault = e.current_contract_address();
    let tail = storage::queue_tail(e);
    let mut head = storage::queue_head(e);
    let mut done = 0u32;

    while head < tail && done < max {
        let Some(r) = storage::request(e, head) else {
            // cancelled: skip
            head += 1;
            continue;
        };
        let assets = AgamaVault::convert_to_assets_floor(e, r.shares);
        if assets > accounting::liquid_assets(e) {
            break;
        }
        // Burn the escrowed shares and set their USDC aside for the owner.
        Base::update(e, Some(&vault), None, r.shares);
        decrease_total_supply(e, r.shares);
        storage::set_pending_shares(e, storage::pending_shares(e) - r.shares);
        storage::set_reserved_assets(e, storage::reserved_assets(e) + assets);
        storage::set_claimable(e, &r.owner, storage::claimable(e, &r.owner) + assets);
        storage::remove_request(e, head);
        RedeemProcessed {
            owner: r.owner,
            id: head,
            shares: r.shares,
            assets,
        }
        .publish(e);
        head += 1;
        done += 1;
    }
    storage::set_queue_head(e, head);
    done
}

pub fn claim(e: &Env, owner: &Address) -> i128 {
    let assets = storage::claimable(e, owner);
    if assets == 0 {
        panic_with_error!(e, VaultError::NothingToClaim);
    }
    storage::set_claimable(e, owner, 0);
    storage::set_reserved_assets(e, storage::reserved_assets(e) - assets);
    token::Client::new(e, &Vault::query_asset(e)).transfer(
        &e.current_contract_address(),
        owner,
        &assets,
    );
    Claimed {
        owner: owner.clone(),
        assets,
    }
    .publish(e);
    assets
}

pub fn get(e: &Env, id: u64) -> Option<RedeemRequest> {
    storage::request(e, id)
}

pub fn bounds(e: &Env) -> (u64, u64) {
    (storage::queue_head(e), storage::queue_tail(e))
}
