#![cfg(test)]
extern crate std;

use agama_interfaces::Asset;
use agama_mock_stablebond::{MockStablebond, MockStablebondClient};
use agama_oracle::{AgamaOracle, AgamaOracleClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};

use crate::{Config, EtherfuseAdapter, EtherfuseAdapterClient};

const USDC: i128 = 10_000_000;
const ONE: i128 = 10_000_000;
const DAY: u64 = 86_400;

struct S<'a> {
    e: Env,
    usdc: TokenClient<'a>,
    mint: StellarAssetClient<'a>,
    pool: MockStablebondClient<'a>,
    oracle: AgamaOracleClient<'a>,
    reporter: Address,
    feed: Asset,
    a: EtherfuseAdapterClient<'a>,
}

fn setup<'a>() -> S<'a> {
    let e = Env::default();
    e.mock_all_auths();
    e.ledger().set_timestamp(10 * DAY);
    let owner = Address::generate(&e);
    let engine = Address::generate(&e);
    let sac = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();
    let pool_id = e.register(MockStablebond, (owner.clone(), sac.clone(), ONE));
    let oracle_id = e.register(
        AgamaOracle,
        (owner.clone(), Asset::Stellar(sac.clone()), 7u32, 3_600u32),
    );
    let oracle = AgamaOracleClient::new(&e, &oracle_id);
    let reporter = Address::generate(&e);
    let feed = Asset::Stellar(pool_id.clone());
    oracle.set_reporter(&reporter, &true);
    oracle.add_feed(&feed, &0); // deterministic NAV: no deviation bound
    oracle.set_price(&reporter, &feed, &ONE, &(10 * DAY));
    let cfg = Config {
        engine,
        asset: sac.clone(),
        pool: pool_id.clone(),
        oracle: oracle_id,
        feed: feed.clone(),
        unit_decimals: 7,
        max_age: 2 * DAY,
        max_slippage_bps: 50,
    };
    let id = e.register(EtherfuseAdapter, (owner, cfg));
    S {
        usdc: TokenClient::new(&e, &sac),
        mint: StellarAssetClient::new(&e, &sac),
        pool: MockStablebondClient::new(&e, &pool_id),
        oracle,
        reporter,
        feed,
        a: EtherfuseAdapterClient::new(&e, &id),
        e,
    }
}

impl S<'_> {
    fn fund(&self, amount: i128) {
        self.mint.mint(&self.a.address, &amount);
        self.a.deposit(&amount);
    }
    fn reprice(&self, price: i128, at: u64) {
        self.e.ledger().set_timestamp(at);
        self.pool.set_price(&price);
        self.oracle
            .set_price(&self.reporter, &self.feed, &price, &at);
        // the pool pays yield from its own reserves
        self.mint.mint(&self.pool.address, &(1_000 * USDC));
    }
}

#[test]
fn deposit_subscribes_immediately() {
    let s = setup();
    s.fund(100 * USDC);
    assert_eq!(s.usdc.balance(&s.a.address), 0);
    assert_eq!(s.a.units(), 100 * ONE);
    assert_eq!(s.a.total_value(), 100 * USDC);
    assert_eq!(s.a.available(), 100 * USDC);
}

#[test]
fn bond_accrual_flows_into_value_and_withdraw_redeems() {
    let s = setup();
    s.fund(100 * USDC);
    s.reprice(ONE * 1004 / 1000, 11 * DAY);
    assert_eq!(s.a.total_value(), 100 * USDC * 1004 / 1000);
    let vault = Address::generate(&s.e);
    let sent = s.a.withdraw(&(50 * USDC), &vault);
    assert_eq!(sent, 50 * USDC);
    assert_eq!(s.usdc.balance(&vault), 50 * USDC);
    // value left ~ 50.4 USDC (rounding dust stays on the adapter)
    let left = s.a.total_value();
    assert!((50 * USDC + 3_999_990..=50 * USDC + 4_000_000).contains(&left));
}

#[test]
fn withdraw_everything() {
    let s = setup();
    s.fund(100 * USDC);
    let vault = Address::generate(&s.e);
    let sent = s.a.withdraw(&(100 * USDC), &vault);
    assert_eq!(sent, 100 * USDC);
    assert_eq!(s.a.units(), 0);
    assert_eq!(s.a.total_value(), 0);
    // asking for more than exists sends what there is
    s.fund(10 * USDC);
    assert_eq!(s.a.withdraw(&(20 * USDC), &vault), 10 * USDC);
}

#[test]
fn pool_price_far_from_oracle_is_refused() {
    let s = setup();
    s.fund(10 * USDC);
    // pool repriced, oracle not: subscription fill is 2% off
    s.pool.set_price(&(ONE * 102 / 100));
    s.mint.mint(&s.a.address, &(10 * USDC));
    assert!(s.a.try_deposit(&(10 * USDC)).is_err());
}

#[test]
fn stale_price_stops_valuation() {
    let s = setup();
    s.fund(10 * USDC);
    s.e.ledger().set_timestamp(12 * DAY + 1);
    assert!(s.a.try_total_value().is_err());
}

#[test]
#[should_panic]
fn only_engine_deposits() {
    let s = setup();
    s.mint.mint(&s.a.address, &USDC);
    s.e.set_auths(&[]);
    s.a.deposit(&USDC);
}
