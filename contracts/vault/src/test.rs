#![cfg(test)]
extern crate std;

use agama_interfaces::{AllocationEngineView, VaultLiquidityClient};
use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env, String,
};

use crate::{AgamaVaultContract, AgamaVaultContractClient};

const USDC: i128 = 10_000_000; // 7 decimals

// ---------------------------------------------------------------- mock engine

#[contracttype]
enum MockKey {
    Value,
    Vault,
}

/// Stands in for the Allocation Engine: reports a value and pulls liquidity.
#[contract]
pub struct MockEngine;

#[contractimpl]
impl MockEngine {
    pub fn set_vault(e: Env, vault: Address) {
        e.storage().instance().set(&MockKey::Vault, &vault);
    }
    pub fn set_value(e: Env, value: i128) {
        e.storage().instance().set(&MockKey::Value, &value);
    }
    /// Pulls `amount` from the vault and books it as deployed value.
    pub fn pull(e: Env, amount: i128) {
        let vault: Address = e.storage().instance().get(&MockKey::Vault).unwrap();
        VaultLiquidityClient::new(&e, &vault).release(&amount, &e.current_contract_address());
        let v: i128 = e.storage().instance().get(&MockKey::Value).unwrap_or(0);
        e.storage().instance().set(&MockKey::Value, &(v + amount));
    }
}

#[contractimpl]
impl AllocationEngineView for MockEngine {
    fn total_value(e: Env) -> i128 {
        e.storage().instance().get(&MockKey::Value).unwrap_or(0)
    }
}

// ---------------------------------------------------------------- setup

struct Setup<'a> {
    e: Env,
    owner: Address,
    usdc: TokenClient<'a>,
    usdc_admin: StellarAssetClient<'a>,
    vault: AgamaVaultContractClient<'a>,
    engine: MockEngineClient<'a>,
}

fn setup<'a>() -> Setup<'a> {
    let e = Env::default();
    e.mock_all_auths();
    let owner = Address::generate(&e);
    let sac = e.register_stellar_asset_contract_v2(owner.clone());
    let usdc = TokenClient::new(&e, &sac.address());
    let usdc_admin = StellarAssetClient::new(&e, &sac.address());
    let vault_id = e.register(
        AgamaVaultContract,
        (
            owner.clone(),
            sac.address(),
            String::from_str(&e, "Agama USD"),
            String::from_str(&e, "agUSD"),
        ),
    );
    let vault = AgamaVaultContractClient::new(&e, &vault_id);
    let engine_id = e.register(MockEngine, ());
    let engine = MockEngineClient::new(&e, &engine_id);
    engine.set_vault(&vault_id);
    vault.set_engine(&engine_id);
    Setup {
        e,
        owner,
        usdc,
        usdc_admin,
        vault,
        engine,
    }
}

impl Setup<'_> {
    fn user(&self, usdc: i128) -> Address {
        let u = Address::generate(&self.e);
        self.usdc_admin.mint(&u, &usdc);
        u
    }
    fn deposit(&self, u: &Address, assets: i128) -> i128 {
        self.vault.deposit(&assets, u, u, u)
    }
    /// Simulates an adapter sending capital back to the vault.
    fn return_capital(&self, amount: i128) {
        self.usdc_admin.mint(&self.vault.address, &amount);
        let v = self.engine.total_value();
        self.engine.set_value(&(v - amount));
    }
}

// ---------------------------------------------------------------- SEP-56 basics

#[test]
fn metadata_and_asset() {
    let s = setup();
    assert_eq!(s.vault.query_asset(), s.usdc.address);
    assert_eq!(s.vault.decimals(), 7);
    assert_eq!(s.vault.symbol(), String::from_str(&s.e, "agUSD"));
    assert_eq!(s.vault.get_owner(), Some(s.owner.clone()));
}

#[test]
fn first_deposit_is_one_to_one() {
    let s = setup();
    let alice = s.user(100 * USDC);
    let shares = s.deposit(&alice, 100 * USDC);
    assert_eq!(shares, 100 * USDC);
    assert_eq!(s.vault.balance(&alice), 100 * USDC);
    assert_eq!(s.vault.total_supply(), 100 * USDC);
    assert_eq!(s.vault.total_assets(), 100 * USDC);
    assert_eq!(s.usdc.balance(&s.vault.address), 100 * USDC);
}

#[test]
fn deployed_capital_counts_in_total_assets() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(60 * USDC));
    assert_eq!(s.usdc.balance(&s.vault.address), 40 * USDC);
    assert_eq!(s.vault.liquid_assets(), 40 * USDC);
    assert_eq!(s.vault.total_assets(), 100 * USDC);
    assert_eq!(s.vault.convert_to_assets(&(100 * USDC)), 100 * USDC);
}

#[test]
fn yield_raises_the_share_price() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(50 * USDC));
    // the credit book earns 10 USDC
    s.engine.set_value(&(60 * USDC));
    assert_eq!(s.vault.total_assets(), 110 * USDC);
    // a later depositor pays the new price
    let bob = s.user(110 * USDC);
    let bob_shares = s.deposit(&bob, 110 * USDC);
    assert!(bob_shares <= 100 * USDC && bob_shares > 99 * USDC);
    // alice's 100 shares are worth ~110 USDC
    let alice_assets = s.vault.preview_redeem(&(100 * USDC));
    assert!(alice_assets > 109 * USDC && alice_assets <= 110 * USDC);
}

#[test]
fn sync_exit_is_bounded_by_liquidity() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(70 * USDC));
    assert_eq!(s.vault.max_withdraw(&alice), 30 * USDC);
    assert_eq!(s.vault.max_redeem(&alice), 30 * USDC);
    assert!(s
        .vault
        .try_withdraw(&(31 * USDC), &alice, &alice, &alice)
        .is_err());
    s.vault.withdraw(&(30 * USDC), &alice, &alice, &alice);
    assert_eq!(s.usdc.balance(&alice), 30 * USDC);
    assert_eq!(s.vault.balance(&alice), 70 * USDC);
}

#[test]
fn redeem_round_trip_without_deployment() {
    let s = setup();
    let alice = s.user(50 * USDC);
    let shares = s.deposit(&alice, 50 * USDC);
    let assets = s.vault.redeem(&shares, &alice, &alice, &alice);
    assert_eq!(assets, 50 * USDC);
    assert_eq!(s.usdc.balance(&alice), 50 * USDC);
    assert_eq!(s.vault.total_supply(), 0);
}

// ---------------------------------------------------------------- queue

#[test]
fn queued_redemption_lifecycle() {
    let s = setup();
    let alice = s.user(100 * USDC);
    let bob = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.deposit(&bob, 100 * USDC);
    s.engine.pull(&(180 * USDC)); // 20 liquid

    let id = s.vault.request_redeem(&(50 * USDC), &alice);
    assert_eq!(id, 0);
    assert_eq!(s.vault.balance(&alice), 50 * USDC);
    assert_eq!(s.vault.pending_shares(), 50 * USDC);
    // supply unchanged: queued shares still earn
    assert_eq!(s.vault.total_supply(), 200 * USDC);

    // a queue closes synchronous exits for everyone
    assert_eq!(s.vault.max_withdraw(&bob), 0);
    assert_eq!(s.vault.max_redeem(&bob), 0);

    // not enough liquidity: nothing processed
    assert_eq!(s.vault.process_redeems(&10), 0);

    // capital comes back from the book
    s.return_capital(40 * USDC);
    assert_eq!(s.vault.process_redeems(&10), 1);
    assert_eq!(s.vault.pending_shares(), 0);
    assert_eq!(s.vault.total_supply(), 150 * USDC);
    assert_eq!(s.vault.claimable(&alice), 50 * USDC);
    assert_eq!(s.vault.reserved_assets(), 50 * USDC);
    // reserved USDC is not part of the vault's assets
    assert_eq!(s.vault.total_assets(), 150 * USDC);
    assert_eq!(s.vault.liquid_assets(), 10 * USDC);

    assert_eq!(s.vault.claim(&alice), 50 * USDC);
    assert_eq!(s.usdc.balance(&alice), 50 * USDC);
    assert_eq!(s.vault.reserved_assets(), 0);
    assert!(s.vault.try_claim(&alice).is_err());
}

#[test]
fn queue_is_first_in_first_out() {
    let s = setup();
    let alice = s.user(100 * USDC);
    let bob = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.deposit(&bob, 100 * USDC);
    s.engine.pull(&(200 * USDC));
    s.vault.request_redeem(&(80 * USDC), &alice);
    s.vault.request_redeem(&(10 * USDC), &bob);
    // 50 back: covers bob's 10 but not alice's 80 at the head
    s.return_capital(50 * USDC);
    assert_eq!(s.vault.process_redeems(&10), 0);
    assert_eq!(s.vault.claimable(&bob), 0);
    s.return_capital(40 * USDC);
    assert_eq!(s.vault.process_redeems(&10), 2);
    assert_eq!(s.vault.claimable(&alice), 80 * USDC);
    assert_eq!(s.vault.claimable(&bob), 10 * USDC);
}

#[test]
fn queued_shares_are_priced_when_processed() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(100 * USDC));
    s.vault.request_redeem(&(100 * USDC), &alice);
    // the book earns 5% while alice waits
    s.engine.set_value(&(105 * USDC));
    s.return_capital(105 * USDC);
    s.vault.process_redeems(&1);
    let claim = s.vault.claimable(&alice);
    assert!(claim > 104 * USDC && claim <= 105 * USDC);
}

#[test]
fn cancel_returns_the_shares() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(100 * USDC));
    let id = s.vault.request_redeem(&(40 * USDC), &alice);
    s.vault.cancel_redeem(&id);
    assert_eq!(s.vault.balance(&alice), 100 * USDC);
    assert_eq!(s.vault.pending_shares(), 0);
    assert!(s.vault.redeem_request(&id).is_none());
    assert!(s.vault.try_cancel_redeem(&id).is_err());
    // cancelled ids are skipped by processing
    s.vault.request_redeem(&(10 * USDC), &alice);
    s.return_capital(10 * USDC);
    assert_eq!(s.vault.process_redeems(&5), 1);
    assert_eq!(s.vault.queue_bounds(), (2, 2));
}

#[test]
fn cannot_queue_more_than_owned() {
    let s = setup();
    let alice = s.user(10 * USDC);
    s.deposit(&alice, 10 * USDC);
    assert!(s.vault.try_request_redeem(&(11 * USDC), &alice).is_err());
    assert!(s.vault.try_request_redeem(&0, &alice).is_err());
}

// ---------------------------------------------------------------- engine access

#[test]
fn release_is_blocked_while_redemptions_are_pending() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(50 * USDC));
    s.vault.request_redeem(&(60 * USDC), &alice);
    assert!(s.engine.try_pull(&(10 * USDC)).is_err());
}

#[test]
fn release_cannot_touch_reserved_assets() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(100 * USDC));
    s.vault.request_redeem(&(50 * USDC), &alice);
    s.return_capital(50 * USDC);
    s.vault.process_redeems(&1);
    assert_eq!(s.vault.liquid_assets(), 0);
    assert!(s.engine.try_pull(&1).is_err());
}

#[test]
#[should_panic]
fn release_requires_the_engine() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.e.set_auths(&[]);
    // a direct call is not authorized by the engine
    s.vault.release(&(10 * USDC), &alice);
}

#[test]
#[should_panic]
fn only_owner_sets_the_engine() {
    let s = setup();
    s.e.set_auths(&[]);
    s.vault.set_engine(&Address::generate(&s.e));
}

// ---------------------------------------------------------------- pause

#[test]
fn pause_stops_entries_and_exits_but_not_claims() {
    let s = setup();
    let alice = s.user(100 * USDC);
    s.deposit(&alice, 100 * USDC);
    s.engine.pull(&(100 * USDC));
    s.vault.request_redeem(&(20 * USDC), &alice);
    s.return_capital(20 * USDC);
    s.vault.process_redeems(&1);

    s.vault.pause(&s.owner);
    assert_eq!(s.vault.max_deposit(&alice), 0);
    assert_eq!(s.vault.max_withdraw(&alice), 0);
    let bob = s.user(10 * USDC);
    assert!(s.vault.try_deposit(&(10 * USDC), &bob, &bob, &bob).is_err());
    assert!(s.vault.try_request_redeem(&USDC, &alice).is_err());
    assert!(s.vault.try_process_redeems(&1).is_err());
    assert_eq!(s.vault.claim(&alice), 20 * USDC);

    s.vault.unpause(&s.owner);
    s.deposit(&bob, 10 * USDC);
}

// ---------------------------------------------------------------- inflation

#[test]
fn donation_attack_is_unprofitable_after_seed_deposit() {
    let s = setup();
    // deployment seeds the vault; the seed shares are never redeemed
    let seeder = s.user(USDC);
    s.deposit(&seeder, USDC);

    let attacker = s.user(1_000 * USDC + 1);
    s.deposit(&attacker, 1);
    // attacker donates to inflate the price
    s.usdc
        .transfer(&attacker, &s.vault.address, &(1_000 * USDC));

    let victim = s.user(500 * USDC);
    s.deposit(&victim, 500 * USDC);

    let attacker_out = s.vault.preview_redeem(&s.vault.balance(&attacker));
    assert!(
        attacker_out < 1_000 * USDC,
        "attacker must lose part of the donation"
    );
    let victim_out = s.vault.preview_redeem(&s.vault.balance(&victim));
    assert!(victim_out > 499 * USDC);
}
