#![no_std]
//! TEST ONLY. A Stablebond pool with an admin-set price, used to exercise the
//! Etherfuse adapter on testnet and in tests. Units and price use 7 decimals.

use agama_interfaces::StablebondPool;
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env};

const ONE: i128 = 10_000_000;

#[contracttype]
enum K {
    Admin,
    Usdc,
    Price,
    Units(Address),
}

#[contract]
pub struct MockStablebond;

fn usdc(e: &Env) -> token::Client<'_> {
    let a: Address = e.storage().instance().get(&K::Usdc).unwrap();
    token::Client::new(e, &a)
}

#[contractimpl]
impl MockStablebond {
    pub fn __constructor(e: Env, admin: Address, usdc: Address, price: i128) {
        e.storage().instance().set(&K::Admin, &admin);
        e.storage().instance().set(&K::Usdc, &usdc);
        e.storage().instance().set(&K::Price, &price);
    }

    pub fn set_price(e: Env, price: i128) {
        let admin: Address = e.storage().instance().get(&K::Admin).unwrap();
        admin.require_auth();
        e.storage().instance().set(&K::Price, &price);
    }

    pub fn price(e: Env) -> i128 {
        e.storage().instance().get(&K::Price).unwrap()
    }
}

#[contractimpl]
impl StablebondPool for MockStablebond {
    fn subscribe(e: Env, from: Address, usdc_amount: i128) -> i128 {
        from.require_auth();
        usdc(&e).transfer(&from, e.current_contract_address(), &usdc_amount);
        let units = usdc_amount * ONE / Self::price(e.clone());
        let held = Self::units(e.clone(), from.clone());
        e.storage()
            .persistent()
            .set(&K::Units(from), &(held + units));
        units
    }

    fn redeem(e: Env, from: Address, units: i128) -> i128 {
        from.require_auth();
        let held = Self::units(e.clone(), from.clone());
        assert!(units <= held, "insufficient units");
        e.storage()
            .persistent()
            .set(&K::Units(from.clone()), &(held - units));
        let out = units * Self::price(e.clone()) / ONE;
        usdc(&e).transfer(&e.current_contract_address(), &from, &out);
        out
    }

    fn units(e: Env, holder: Address) -> i128 {
        e.storage().persistent().get(&K::Units(holder)).unwrap_or(0)
    }
}
