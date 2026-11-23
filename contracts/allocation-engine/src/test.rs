#![cfg(test)]
extern crate std;

use agama_interfaces::{StrategyAdapter, VaultLiquidity};
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short,
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env, Symbol,
};

use crate::{AllocationEngine, AllocationEngineClient, MAX_ADAPTERS};

const USDC: i128 = 10_000_000;

// ---------------------------------------------------------------- mocks

#[contracttype]
enum K {
    Asset,
    Engine,
    Extra,
}

/// Vault stand-in: liquid = USDC balance, release only for the engine.
#[contract]
pub struct MockVault;

#[contractimpl]
impl MockVault {
    pub fn __constructor(e: Env, asset: Address) {
        e.storage().instance().set(&K::Asset, &asset);
    }
    pub fn set_engine(e: Env, engine: Address) {
        e.storage().instance().set(&K::Engine, &engine);
    }
}

#[contractimpl]
impl VaultLiquidity for MockVault {
    fn liquid_assets(e: Env) -> i128 {
        let asset: Address = e.storage().instance().get(&K::Asset).unwrap();
        TokenClient::new(&e, &asset).balance(&e.current_contract_address())
    }
    fn release(e: Env, amount: i128, to: Address) {
        let engine: Address = e.storage().instance().get(&K::Engine).unwrap();
        engine.require_auth();
        let asset: Address = e.storage().instance().get(&K::Asset).unwrap();
        TokenClient::new(&e, &asset).transfer(&e.current_contract_address(), &to, &amount);
    }
}

/// Adapter stand-in: value = USDC balance + `extra` (unrealised yield).
#[contract]
pub struct MockAdapter;

#[contractimpl]
impl MockAdapter {
    pub fn __constructor(e: Env, asset: Address) {
        e.storage().instance().set(&K::Asset, &asset);
    }
    pub fn set_extra(e: Env, v: i128) {
        e.storage().instance().set(&K::Extra, &v);
    }
}

#[contractimpl]
impl StrategyAdapter for MockAdapter {
    fn asset(e: Env) -> Address {
        e.storage().instance().get(&K::Asset).unwrap()
    }
    fn total_value(e: Env) -> i128 {
        let extra: i128 = e.storage().instance().get(&K::Extra).unwrap_or(0);
        Self::available(e) + extra
    }
    fn available(e: Env) -> i128 {
        let asset: Address = e.storage().instance().get(&K::Asset).unwrap();
        TokenClient::new(&e, &asset).balance(&e.current_contract_address())
    }
    fn deposit(_e: Env, _amount: i128) {}
    fn withdraw(e: Env, amount: i128, to: Address) -> i128 {
        let asset: Address = e.storage().instance().get(&K::Asset).unwrap();
        TokenClient::new(&e, &asset).transfer(&e.current_contract_address(), &to, &amount);
        amount
    }
}

// ---------------------------------------------------------------- setup

struct S<'a> {
    e: Env,
    owner: Address,
    usdc: Address,
    vault: Address,
    engine: AllocationEngineClient<'a>,
}

fn setup<'a>(vault_usdc: i128) -> S<'a> {
    let e = Env::default();
    e.mock_all_auths();
    let owner = Address::generate(&e);
    let curator = Address::generate(&e);
    let usdc = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();
    let vault = e.register(MockVault, (usdc.clone(),));
    let engine_id = e.register(
        AllocationEngine,
        (owner.clone(), curator, vault.clone(), usdc.clone()),
    );
    MockVaultClient::new(&e, &vault).set_engine(&engine_id);
    StellarAssetClient::new(&e, &usdc).mint(&vault, &vault_usdc);
    let engine = AllocationEngineClient::new(&e, &engine_id);
    S {
        e,
        owner,
        usdc,
        vault,
        engine,
    }
}

impl S<'_> {
    fn adapter(&self, originator: Symbol, jurisdiction: Symbol, cap: u32) -> Address {
        let a = self.e.register(MockAdapter, (self.usdc.clone(),));
        self.engine
            .register_adapter(&a, &originator, &jurisdiction, &cap);
        a
    }
    fn bal(&self, a: &Address) -> i128 {
        TokenClient::new(&self.e, &self.usdc).balance(a)
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn allocate_and_deallocate_move_capital() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("TENKA"), symbol_short!("LU"), 10_000);
    s.engine.allocate(&a, &(60 * USDC));
    assert_eq!(s.bal(&a), 60 * USDC);
    assert_eq!(s.bal(&s.vault), 40 * USDC);
    assert_eq!(s.engine.total_value(), 60 * USDC);
    s.engine.deallocate(&a, &(25 * USDC));
    assert_eq!(s.bal(&s.vault), 65 * USDC);
    assert_eq!(s.engine.total_value(), 35 * USDC);
}

#[test]
fn total_value_includes_unrealised_yield() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("TENKA"), symbol_short!("LU"), 10_000);
    s.engine.allocate(&a, &(50 * USDC));
    MockAdapterClient::new(&s.e, &a).set_extra(&(3 * USDC));
    assert_eq!(s.engine.total_value(), 53 * USDC);
}

#[test]
fn reserve_floor_is_enforced() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("TENKA"), symbol_short!("LU"), 10_000);
    s.engine.set_reserve_floor(&2_000); // 20% stays liquid
    assert!(s.engine.try_allocate(&a, &(81 * USDC)).is_err());
    s.engine.allocate(&a, &(80 * USDC));
    assert!(s.engine.try_allocate(&a, &1).is_err());
}

#[test]
fn adapter_cap_is_enforced() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("TENKA"), symbol_short!("LU"), 3_000);
    assert!(s.engine.try_allocate(&a, &(31 * USDC)).is_err());
    s.engine.allocate(&a, &(30 * USDC));
    s.engine.set_adapter_cap(&a, &5_000);
    s.engine.allocate(&a, &(20 * USDC));
}

#[test]
fn originator_cap_spans_adapters() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("TENKA"), symbol_short!("LU"), 10_000);
    let b = s.adapter(symbol_short!("TENKA"), symbol_short!("FR"), 10_000);
    s.engine.set_originator_cap(&symbol_short!("TENKA"), &5_000);
    s.engine.allocate(&a, &(30 * USDC));
    assert!(s.engine.try_allocate(&b, &(21 * USDC)).is_err());
    s.engine.allocate(&b, &(20 * USDC));
}

#[test]
fn jurisdiction_cap_spans_adapters() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("TENKA"), symbol_short!("LU"), 10_000);
    let b = s.adapter(symbol_short!("EFUSE"), symbol_short!("LU"), 10_000);
    s.engine.set_jurisdiction_cap(&symbol_short!("LU"), &4_000);
    s.engine.allocate(&a, &(25 * USDC));
    assert!(s.engine.try_allocate(&b, &(16 * USDC)).is_err());
    s.engine.allocate(&b, &(15 * USDC));
}

#[test]
fn caps_use_total_assets_including_deployed_value() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("TENKA"), symbol_short!("LU"), 5_000);
    s.engine.allocate(&a, &(50 * USDC));
    // the position gains 10: total = 110, a = 60 > 55 cap
    MockAdapterClient::new(&s.e, &a).set_extra(&(10 * USDC));
    assert!(s.engine.try_allocate(&a, &1).is_err());
}

#[test]
fn registry_rules() {
    let s = setup(0);
    let a = s.adapter(symbol_short!("A"), symbol_short!("X"), 10_000);
    assert!(s
        .engine
        .try_register_adapter(&a, &symbol_short!("A"), &symbol_short!("X"), &10_000)
        .is_err());
    // wrong asset
    let other =
        s.e.register_stellar_asset_contract_v2(s.owner.clone())
            .address();
    let wrong = s.e.register(MockAdapter, (other,));
    assert!(s
        .engine
        .try_register_adapter(&wrong, &symbol_short!("A"), &symbol_short!("X"), &10_000)
        .is_err());
    // invalid bps
    let c = s.e.register(MockAdapter, (s.usdc.clone(),));
    assert!(s
        .engine
        .try_register_adapter(&c, &symbol_short!("A"), &symbol_short!("X"), &10_001)
        .is_err());
    // bounded registry
    for _ in 1..MAX_ADAPTERS {
        s.adapter(symbol_short!("A"), symbol_short!("X"), 10_000);
    }
    let extra = s.e.register(MockAdapter, (s.usdc.clone(),));
    assert!(s
        .engine
        .try_register_adapter(&extra, &symbol_short!("A"), &symbol_short!("X"), &10_000)
        .is_err());
}

#[test]
fn remove_only_empty_adapters() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("A"), symbol_short!("X"), 10_000);
    s.engine.allocate(&a, &(10 * USDC));
    assert!(s.engine.try_remove_adapter(&a).is_err());
    s.engine.deallocate(&a, &(10 * USDC));
    s.engine.remove_adapter(&a);
    assert_eq!(s.engine.adapters().len(), 0);
    assert!(s.engine.try_allocate(&a, &1).is_err());
}

#[test]
fn pause_blocks_allocate_not_deallocate() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("A"), symbol_short!("X"), 10_000);
    s.engine.allocate(&a, &(10 * USDC));
    s.engine.pause(&s.owner);
    assert!(s.engine.try_allocate(&a, &1).is_err());
    s.engine.deallocate(&a, &(10 * USDC));
}

#[test]
#[should_panic]
fn only_curator_allocates() {
    let s = setup(100 * USDC);
    let a = s.adapter(symbol_short!("A"), symbol_short!("X"), 10_000);
    s.e.set_auths(&[]);
    s.engine.allocate(&a, &1);
}
