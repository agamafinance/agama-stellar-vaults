#![cfg(test)]
extern crate std;

use agama_interfaces::Asset;
use agama_oracle::{AgamaOracle, AgamaOracleClient};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};

use crate::{Config, PrivateCreditAdapter, PrivateCreditAdapterClient};

const USDC: i128 = 10_000_000; // 7 dp on Stellar
const TK: i128 = 1_000_000; // tkUSD: 6 dp
const PRICE_ONE: i128 = 10_000_000; // oracle: 7 dp
const DAY: u64 = 86_400;

struct S<'a> {
    e: Env,
    engine: Address,
    operator: Address,
    usdc: TokenClient<'a>,
    mint: StellarAssetClient<'a>,
    oracle: AgamaOracleClient<'a>,
    reporter: Address,
    a: PrivateCreditAdapterClient<'a>,
}

fn feed() -> Asset {
    Asset::Other(symbol_short!("TKUSD"))
}

fn setup<'a>() -> S<'a> {
    let e = Env::default();
    e.mock_all_auths();
    e.ledger().set_timestamp(100 * DAY);
    let owner = Address::generate(&e);
    let engine = Address::generate(&e);
    let operator = Address::generate(&e);
    let sac = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();
    let oracle_id = e.register(
        AgamaOracle,
        (owner.clone(), Asset::Stellar(sac.clone()), 7u32, DAY as u32),
    );
    let oracle = AgamaOracleClient::new(&e, &oracle_id);
    let reporter = Address::generate(&e);
    oracle.set_reporter(&reporter, &true);
    oracle.add_feed(&feed(), &500);
    oracle.set_price(&reporter, &feed(), &PRICE_ONE, &(100 * DAY));
    let cfg = Config {
        engine: engine.clone(),
        asset: sac.clone(),
        operator: operator.clone(),
        oracle: oracle_id,
        feed: feed(),
        unit_decimals: 6,
        max_age: 14 * DAY,
        max_slippage_bps: 100,
    };
    let id = e.register(PrivateCreditAdapter, (owner, cfg));
    S {
        engine,
        operator,
        usdc: TokenClient::new(&e, &sac),
        mint: StellarAssetClient::new(&e, &sac),
        oracle,
        reporter,
        a: PrivateCreditAdapterClient::new(&e, &id),
        e,
    }
}

impl S<'_> {
    /// The vault releases `amount` to the adapter and the engine notifies it.
    fn fund(&self, amount: i128) {
        self.mint.mint(&self.a.address, &amount);
        self.a.deposit(&amount);
    }
    fn price(&self, price: i128, at: u64) {
        self.e.ledger().set_timestamp(at);
        self.oracle.set_price(&self.reporter, &feed(), &price, &at);
    }
}

#[test]
fn full_cycle_keeps_value_continuous() {
    let s = setup();
    s.fund(100 * USDC);
    assert_eq!(s.a.total_value(), 100 * USDC);

    s.a.dispatch(&(100 * USDC));
    assert_eq!(s.usdc.balance(&s.operator), 100 * USDC);
    assert_eq!(s.a.in_transit(), 100 * USDC);
    assert_eq!(s.a.total_value(), 100 * USDC);

    s.a.confirm_subscription(&(100 * USDC), &(100 * TK));
    assert_eq!(s.a.in_transit(), 0);
    assert_eq!(s.a.units(), 100 * TK);
    assert_eq!(s.a.total_value(), 100 * USDC);

    // a week of yield: tkUSD +0.2%
    s.price(PRICE_ONE * 1002 / 1000, 107 * DAY);
    assert_eq!(s.a.total_value(), 100 * USDC * 1002 / 1000);

    // half redeemed, USDC comes back
    let back = 50 * USDC * 1002 / 1000;
    s.a.settle_redemption(&(50 * TK), &back);
    assert_eq!(s.a.units(), 50 * TK);
    assert_eq!(s.a.available(), back);
    assert_eq!(s.a.total_value(), 100 * USDC * 1002 / 1000);

    // engine sends it to the vault
    let vault = Address::generate(&s.e);
    s.a.withdraw(&back, &vault);
    assert_eq!(s.usdc.balance(&vault), back);
}

#[test]
fn stale_or_missing_price_stops_valuation() {
    let s = setup();
    s.fund(10 * USDC);
    s.a.dispatch(&(10 * USDC));
    s.a.confirm_subscription(&(10 * USDC), &(10 * TK));
    s.e.ledger().set_timestamp(100 * DAY + 14 * DAY + 1);
    assert!(s.a.try_total_value().is_err());
    // fresh price restores it
    s.price(PRICE_ONE, 100 * DAY + 14 * DAY + 2);
    assert_eq!(s.a.total_value(), 10 * USDC);
}

#[test]
fn fills_must_match_the_oracle_price() {
    let s = setup();
    s.fund(100 * USDC);
    s.a.dispatch(&(100 * USDC));
    // 100 USDC for 95 units at price 1.0 = 5% off, above 1%
    assert!(s
        .a
        .try_confirm_subscription(&(100 * USDC), &(95 * TK))
        .is_err());
    s.a.confirm_subscription(&(100 * USDC), &(99_500_000)); // 0.5% off
                                                            // redemption paying far below fair value is refused
    s.mint.mint(&s.operator, &(100 * USDC));
    assert!(s.a.try_settle_redemption(&(50 * TK), &(40 * USDC)).is_err());
}

#[test]
fn buckets_cannot_go_negative() {
    let s = setup();
    s.fund(10 * USDC);
    assert!(s.a.try_dispatch(&(11 * USDC)).is_err());
    s.a.dispatch(&(10 * USDC));
    assert!(s
        .a
        .try_confirm_subscription(&(11 * USDC), &(11 * TK))
        .is_err());
    assert!(s.a.try_cancel_transit(&(11 * USDC)).is_err());
    s.a.cancel_transit(&(4 * USDC));
    assert_eq!(s.a.available(), 4 * USDC);
    assert_eq!(s.a.total_value(), 10 * USDC);
    s.a.confirm_subscription(&(6 * USDC), &(6 * TK));
    assert!(s.a.try_settle_redemption(&(7 * TK), &(7 * USDC)).is_err());
    assert!(s.a.try_withdraw(&(5 * USDC), &s.engine).is_err());
}

#[test]
fn write_down_flows_into_value() {
    let s = setup();
    s.fund(100 * USDC);
    s.a.dispatch(&(100 * USDC));
    s.a.confirm_subscription(&(100 * USDC), &(100 * TK));
    // a default marks the fund down 20%: beyond the 5% feed bound, the
    // oracle owner forces it
    s.e.ledger().set_timestamp(101 * DAY);
    s.oracle
        .force_price(&feed(), &(PRICE_ONE * 80 / 100), &(101 * DAY));
    assert_eq!(s.a.total_value(), 80 * USDC);
}

#[test]
#[should_panic]
fn only_operator_dispatches() {
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
    s.a.withdraw(&USDC, &s.operator);
}
