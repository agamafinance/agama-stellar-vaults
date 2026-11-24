#![cfg(test)]
extern crate std;

use agama_interfaces::Asset;
use agama_oracle::{AgamaOracle, AgamaOracleClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};

use crate::{Config, EtherfuseAdapter, EtherfuseAdapterClient};

const USDC: i128 = 10_000_000; // 7 dp
const BOND: i128 = 10_000_000; // 7 dp, like CETES
/// CETES ≈ 0.0621 USD, Etherfuse publishes 14 decimals.
const CETES_PRICE: i128 = 6_210_000_000_000;
const DAY: u64 = 86_400;

struct S<'a> {
    e: Env,
    engine: Address,
    operator: Address,
    usdc: TokenClient<'a>,
    mint_usdc: StellarAssetClient<'a>,
    bond: TokenClient<'a>,
    mint_bond: StellarAssetClient<'a>,
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
    let operator = Address::generate(&e);
    let usdc = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();
    let bond = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();
    // same shape as Etherfuse's aggregator: base USD, 14 decimals
    let oracle_id = e.register(
        AgamaOracle,
        (
            owner.clone(),
            Asset::Other(soroban_sdk::symbol_short!("USD")),
            14u32,
            300u32,
        ),
    );
    let oracle = AgamaOracleClient::new(&e, &oracle_id);
    let reporter = Address::generate(&e);
    let feed = Asset::Stellar(bond.clone());
    oracle.set_reporter(&reporter, &true);
    oracle.add_feed(&feed, &200);
    oracle.set_price(&reporter, &feed, &CETES_PRICE, &(10 * DAY));
    let cfg = Config {
        engine: engine.clone(),
        asset: usdc.clone(),
        bond: bond.clone(),
        operator: operator.clone(),
        oracle: oracle_id,
        feed: feed.clone(),
        max_age: 3_600,
        max_slippage_bps: 100,
    };
    let id = e.register(EtherfuseAdapter, (owner, cfg));
    S {
        engine,
        operator,
        usdc: TokenClient::new(&e, &usdc),
        mint_usdc: StellarAssetClient::new(&e, &usdc),
        bond: TokenClient::new(&e, &bond),
        mint_bond: StellarAssetClient::new(&e, &bond),
        oracle,
        reporter,
        feed,
        a: EtherfuseAdapterClient::new(&e, &id),
        e,
    }
}

/// Bonds bought with `usdc` (7 dp) at `price` (14 dp).
fn bonds_for(usdc: i128, price: i128) -> i128 {
    usdc * 100_000_000_000_000 / price
}

impl S<'_> {
    fn fund(&self, amount: i128) {
        self.mint_usdc.mint(&self.a.address, &amount);
        self.a.deposit(&amount);
    }
    /// The operator buys through Etherfuse and delivers.
    fn buy(&self, usdc: i128) -> i128 {
        self.a.dispatch(&usdc);
        let bonds = bonds_for(usdc, CETES_PRICE);
        self.mint_bond.mint(&self.operator, &bonds);
        self.a.confirm_purchase(&usdc, &bonds);
        bonds
    }
}

#[test]
fn purchase_puts_the_bonds_on_the_adapter() {
    let s = setup();
    s.fund(100 * USDC);
    s.a.dispatch(&(100 * USDC));
    assert_eq!(s.a.usdc_in_transit(), 100 * USDC);
    assert_eq!(s.a.total_value(), 100 * USDC);
    let bonds = bonds_for(100 * USDC, CETES_PRICE);
    s.mint_bond.mint(&s.operator, &bonds);
    s.a.confirm_purchase(&(100 * USDC), &bonds);
    assert_eq!(s.bond.balance(&s.a.address), bonds);
    assert_eq!(s.bond.balance(&s.operator), 0);
    assert_eq!(s.a.usdc_in_transit(), 0);
    let v = s.a.total_value();
    assert!((v - 100 * USDC).abs() <= 1, "value {v}");
    assert_eq!(s.a.available(), 0);
}

#[test]
fn accrual_and_sale_round_trip() {
    let s = setup();
    s.fund(100 * USDC);
    let bonds = s.buy(100 * USDC);
    // CETES accrues 0.5%
    s.e.ledger().set_timestamp(10 * DAY + 600);
    let p = CETES_PRICE * 1005 / 1000;
    s.oracle
        .set_price(&s.reporter, &s.feed, &p, &(10 * DAY + 600));
    let v = s.a.total_value();
    assert!((v - 100 * USDC * 1005 / 1000).abs() <= 2, "value {v}");

    s.a.dispatch_bonds(&bonds);
    assert_eq!(s.a.bonds_in_transit(), bonds);
    assert_eq!(s.a.total_value(), v);
    let proceeds = 100 * USDC * 1005 / 1000;
    s.mint_usdc.mint(&s.operator, &proceeds);
    s.a.settle_sale(&bonds, &proceeds);
    assert_eq!(s.a.bonds_in_transit(), 0);
    assert_eq!(s.a.available(), proceeds);

    let vault = Address::generate(&s.e);
    s.a.withdraw(&proceeds, &vault);
    assert_eq!(s.usdc.balance(&vault), proceeds);
    assert_eq!(s.a.total_value(), 0);
}

#[test]
fn cancellations_restore_the_position() {
    let s = setup();
    s.fund(50 * USDC);
    s.a.dispatch(&(50 * USDC));
    s.a.cancel_transit(&(20 * USDC));
    assert_eq!(s.a.available(), 20 * USDC);
    assert_eq!(s.a.usdc_in_transit(), 30 * USDC);
    let bonds = bonds_for(30 * USDC, CETES_PRICE);
    s.mint_bond.mint(&s.operator, &bonds);
    s.a.confirm_purchase(&(30 * USDC), &bonds);
    s.a.dispatch_bonds(&bonds);
    s.a.cancel_sale(&bonds);
    assert_eq!(s.a.bonds(), bonds);
    assert_eq!(s.a.bonds_in_transit(), 0);
}

#[test]
fn fills_far_from_the_oracle_are_refused() {
    let s = setup();
    s.fund(100 * USDC);
    s.a.dispatch(&(100 * USDC));
    let fair = bonds_for(100 * USDC, CETES_PRICE);
    s.mint_bond.mint(&s.operator, &fair);
    // 3% fewer bonds than the price implies
    assert!(s
        .a
        .try_confirm_purchase(&(100 * USDC), &(fair * 97 / 100))
        .is_err());
    s.a.confirm_purchase(&(100 * USDC), &fair);
    s.a.dispatch_bonds(&fair);
    s.mint_usdc.mint(&s.operator, &(100 * USDC));
    assert!(s.a.try_settle_sale(&fair, &(95 * USDC)).is_err());
}

#[test]
fn buckets_and_balances_bound_every_move() {
    let s = setup();
    s.fund(10 * USDC);
    assert!(s.a.try_dispatch(&(11 * USDC)).is_err());
    s.a.dispatch(&(10 * USDC));
    assert!(s.a.try_cancel_transit(&(11 * USDC)).is_err());
    assert!(s.a.try_dispatch_bonds(&1).is_err());
    assert!(s.a.try_settle_sale(&1, &1).is_err());
    assert!(s.a.try_withdraw(&1, &s.engine).is_err());
    // the operator must actually hold the bonds it confirms
    assert!(s
        .a
        .try_confirm_purchase(&(10 * USDC), &bonds_for(10 * USDC, CETES_PRICE))
        .is_err());
}

#[test]
fn stale_price_stops_valuation() {
    let s = setup();
    s.fund(10 * USDC);
    s.buy(10 * USDC);
    s.e.ledger().set_timestamp(10 * DAY + 3_601);
    assert!(s.a.try_total_value().is_err());
    // idle USDC alone needs no price
    let t = setup();
    t.fund(10 * USDC);
    t.e.ledger().set_timestamp(20 * DAY);
    assert_eq!(t.a.total_value(), 10 * USDC);
}

#[test]
#[should_panic]
fn only_operator_buys() {
    let s = setup();
    s.fund(10 * USDC);
    s.e.set_auths(&[]);
    s.a.dispatch(&USDC);
}

#[test]
#[should_panic]
fn only_engine_withdraws() {
    let s = setup();
    s.fund(10 * USDC);
    s.e.set_auths(&[]);
    s.a.withdraw(&USDC, &s.engine);
}

#[test]
fn constructor_rejects_bad_config() {
    let e = Env::default();
    let owner = Address::generate(&e);
    let usdc = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();
    let cfg = Config {
        engine: owner.clone(),
        asset: usdc.clone(),
        bond: usdc.clone(),
        operator: owner.clone(),
        oracle: owner.clone(),
        feed: Asset::Stellar(usdc),
        max_age: 1,
        max_slippage_bps: 1,
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.register(EtherfuseAdapter, (owner.clone(), cfg));
    }));
    assert!(r.is_err());
    let _ = BOND;
}
