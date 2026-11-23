#![cfg(test)]
extern crate std;

use agama_interfaces::{Asset, PriceData, Sep40OracleClient};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger},
    Address, Env,
};

use crate::{AgamaOracle, AgamaOracleClient, HISTORY};

const ONE: i128 = 10_000_000; // 7 decimals

fn setup<'a>() -> (Env, Address, Address, AgamaOracleClient<'a>) {
    let e = Env::default();
    e.mock_all_auths();
    e.ledger().set_timestamp(1_000_000);
    let owner = Address::generate(&e);
    let usdc = Address::generate(&e);
    let id = e.register(
        AgamaOracle,
        (owner.clone(), Asset::Stellar(usdc), 7u32, 86_400u32),
    );
    let c = AgamaOracleClient::new(&e, &id);
    let reporter = Address::generate(&e);
    c.set_reporter(&reporter, &true);
    c.add_feed(&tkusd(), &200); // 2%
    (e, owner, reporter, c)
}

fn tkusd() -> Asset {
    Asset::Other(symbol_short!("TKUSD"))
}

#[test]
fn sep40_metadata() {
    let (e, _, _, c) = setup();
    let r = Sep40OracleClient::new(&e, &c.address);
    assert_eq!(r.decimals(), 7);
    assert_eq!(r.resolution(), 86_400);
    assert_eq!(r.assets().len(), 1);
    assert!(r.lastprice(&tkusd()).is_none());
}

#[test]
fn reporter_pushes_and_sep40_reads() {
    let (e, _, rep, c) = setup();
    c.set_price(&rep, &tkusd(), &ONE, &999_000);
    c.set_price(&rep, &tkusd(), &(ONE + ONE / 100), &999_500);
    let r = Sep40OracleClient::new(&e, &c.address);
    assert_eq!(
        r.lastprice(&tkusd()),
        Some(PriceData {
            price: ONE + ONE / 100,
            timestamp: 999_500
        })
    );
    assert_eq!(
        r.price(&tkusd(), &999_100),
        Some(PriceData {
            price: ONE,
            timestamp: 999_000
        })
    );
    assert!(r.price(&tkusd(), &10).is_none());
    let hist = r.prices(&tkusd(), &5).unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist.get(0).unwrap().timestamp, 999_500);
}

#[test]
fn deviation_bound_and_forced_override() {
    let (_, _, rep, c) = setup();
    c.set_price(&rep, &tkusd(), &ONE, &999_000);
    // +3% > 2% bound
    assert!(c
        .try_set_price(&rep, &tkusd(), &(ONE * 103 / 100), &999_001)
        .is_err());
    // owner can force a write-down
    c.force_price(&(tkusd()), &(ONE * 90 / 100), &999_002);
    assert_eq!(c.lastprice(&tkusd()).unwrap().price, ONE * 90 / 100);
}

#[test]
fn timestamps_must_increase_and_not_be_in_the_future() {
    let (_, _, rep, c) = setup();
    c.set_price(&rep, &tkusd(), &ONE, &999_000);
    assert!(c.try_set_price(&rep, &tkusd(), &ONE, &999_000).is_err());
    assert!(c.try_set_price(&rep, &tkusd(), &ONE, &2_000_000).is_err());
}

#[test]
fn rejects_unknown_feeds_bad_prices_and_strangers() {
    let (e, _, rep, c) = setup();
    let other = Asset::Other(symbol_short!("NOPE"));
    assert!(c.try_set_price(&rep, &other, &ONE, &999_000).is_err());
    assert!(c.try_set_price(&rep, &tkusd(), &0, &999_000).is_err());
    let stranger = Address::generate(&e);
    assert!(c
        .try_set_price(&stranger, &tkusd(), &ONE, &999_000)
        .is_err());
    c.set_reporter(&rep, &false);
    assert!(c.try_set_price(&rep, &tkusd(), &ONE, &999_000).is_err());
}

#[test]
fn history_is_a_bounded_ring() {
    let (e, _, rep, c) = setup();
    for i in 0..(HISTORY as u64 + 5) {
        c.set_price(&rep, &tkusd(), &ONE, &(900_000 + i));
    }
    let r = Sep40OracleClient::new(&e, &c.address);
    assert_eq!(r.prices(&tkusd(), &100).unwrap().len(), HISTORY);
    // the oldest records were overwritten
    assert!(r.price(&tkusd(), &900_001).is_none());
    assert_eq!(
        r.lastprice(&tkusd()).unwrap().timestamp,
        900_000 + HISTORY as u64 + 4
    );
}

#[test]
#[should_panic]
fn only_owner_adds_feeds() {
    let (e, _, _, c) = setup();
    e.set_auths(&[]);
    c.add_feed(&Asset::Other(symbol_short!("X")), &0);
}
