//! Property tests: random sequences of user, engine and market actions, with
//! the vault invariants checked after every step.
extern crate std;

use proptest::prelude::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env, String,
};
use std::{vec, vec::Vec};

use crate::test::{AgamaVaultContract, AgamaVaultContractClient, MockEngine, MockEngineClient};

const USDC: i128 = 10_000_000;
const USERS: usize = 4;

#[derive(Clone, Debug)]
enum Op {
    Deposit(usize, i128),
    Withdraw(usize, u8),
    Redeem(usize, u8),
    Request(usize, u8),
    Cancel(u8),
    Process(u8),
    Claim(usize),
    Pull(u8),
    Return(u8),
    Gain(i128),
    Loss(u8),
    Pause(bool),
    Transfer(usize, usize, u8),
}

fn op() -> impl Strategy<Value = Op> {
    let u = 0..USERS;
    let pct = 1u8..=100;
    prop_oneof![
        3 => (u.clone(), 1i128..5_000).prop_map(|(u, a)| Op::Deposit(u, a * USDC / 10)),
        1 => (u.clone(), pct.clone()).prop_map(|(u, p)| Op::Withdraw(u, p)),
        1 => (u.clone(), pct.clone()).prop_map(|(u, p)| Op::Redeem(u, p)),
        2 => (u.clone(), pct.clone()).prop_map(|(u, p)| Op::Request(u, p)),
        1 => any::<u8>().prop_map(Op::Cancel),
        2 => (1u8..10).prop_map(Op::Process),
        1 => u.clone().prop_map(Op::Claim),
        2 => pct.clone().prop_map(Op::Pull),
        2 => pct.clone().prop_map(Op::Return),
        1 => (1i128..200).prop_map(|g| Op::Gain(g * USDC / 10)),
        1 => (1u8..30).prop_map(Op::Loss),
        1 => any::<bool>().prop_map(Op::Pause),
        1 => (u.clone(), u, pct).prop_map(|(a, b, p)| Op::Transfer(a, b, p)),
    ]
}

struct World<'a> {
    e: Env,
    owner: Address,
    users: Vec<Address>,
    usdc: TokenClient<'a>,
    mint: StellarAssetClient<'a>,
    v: AgamaVaultContractClient<'a>,
    eng: MockEngineClient<'a>,
    /// USDC put in by users, net of what came back to them.
    net_in: i128,
    /// Market P&L booked on the engine.
    pnl: i128,
}

fn world<'a>() -> World<'a> {
    let e = Env::default();
    e.mock_all_auths();
    e.cost_estimate().budget().reset_unlimited();
    let owner = Address::generate(&e);
    let sac = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();
    let v_id = e.register(
        AgamaVaultContract,
        (
            owner.clone(),
            sac.clone(),
            String::from_str(&e, "Agama USD"),
            String::from_str(&e, "agUSD"),
        ),
    );
    let eng_id = e.register(MockEngine, ());
    let eng = MockEngineClient::new(&e, &eng_id);
    eng.set_vault(&v_id);
    let v = AgamaVaultContractClient::new(&e, &v_id);
    v.set_engine(&eng_id);
    let mint = StellarAssetClient::new(&e, &sac);
    let users: Vec<Address> = (0..USERS).map(|_| Address::generate(&e)).collect();
    for u in &users {
        mint.mint(u, &(1_000_000 * USDC));
    }
    // seed deposit, never redeemed
    mint.mint(&owner, &USDC);
    v.deposit(&USDC, &owner, &owner, &owner);
    World {
        usdc: TokenClient::new(&e, &sac),
        mint,
        v,
        eng,
        owner,
        users,
        net_in: USDC,
        pnl: 0,
        e,
    }
}

impl World<'_> {
    fn bal(&self, u: usize) -> i128 {
        self.v.balance(&self.users[u])
    }

    fn apply(&mut self, op: &Op) {
        let v = &self.v;
        match *op {
            Op::Deposit(u, a) => {
                let who = &self.users[u];
                if v.try_deposit(&a, who, who, who).is_ok() {
                    self.net_in += a;
                }
            }
            Op::Withdraw(u, p) => {
                let who = &self.users[u];
                let a = v.max_withdraw(who) * p as i128 / 100;
                if a > 0 && v.try_withdraw(&a, who, who, who).is_ok() {
                    self.net_in -= a;
                }
            }
            Op::Redeem(u, p) => {
                let who = &self.users[u];
                let s = v.max_redeem(who) * p as i128 / 100;
                if s > 0 {
                    if let Ok(Ok(a)) = v.try_redeem(&s, who, who, who) {
                        self.net_in -= a;
                    }
                }
            }
            Op::Request(u, p) => {
                let s = self.bal(u) * p as i128 / 100;
                if s > 0 {
                    let _ = v.try_request_redeem(&s, &self.users[u]);
                }
            }
            Op::Cancel(i) => {
                let (h, t) = v.queue_bounds();
                if t > h {
                    let id = h + (i as u64 % (t - h));
                    let _ = v.try_cancel_redeem(&id);
                }
            }
            Op::Process(n) => {
                let _ = v.try_process_redeems(&(n as u32));
            }
            Op::Claim(u) => {
                if let Ok(Ok(a)) = v.try_claim(&self.users[u]) {
                    self.net_in -= a;
                }
            }
            Op::Pull(p) => {
                let a = v.liquid_assets() * p as i128 / 100;
                if a > 0 {
                    let _ = self.eng.try_pull(&a);
                }
            }
            Op::Return(p) => {
                let a = self.eng.total_value() * p as i128 / 100;
                if a > 0 {
                    self.mint.mint(&v.address, &a);
                    self.eng.set_value(&(self.eng.total_value() - a));
                }
            }
            Op::Gain(g) => {
                if self.eng.total_value() > 0 {
                    self.eng.set_value(&(self.eng.total_value() + g));
                    self.pnl += g;
                }
            }
            Op::Loss(p) => {
                let l = self.eng.total_value() * p as i128 / 100;
                self.eng.set_value(&(self.eng.total_value() - l));
                self.pnl -= l;
            }
            Op::Pause(on) => {
                if on {
                    let _ = v.try_pause(&self.owner);
                } else {
                    let _ = v.try_unpause(&self.owner);
                }
            }
            Op::Transfer(a, b, p) => {
                let s = self.bal(a) * p as i128 / 100;
                if s > 0 {
                    let _ = v.try_transfer(&self.users[a], &self.users[b], &s);
                }
            }
        }
    }

    fn check(&self) {
        let v = &self.v;
        let liquid = v.liquid_assets();
        let deployed = self.eng.total_value();
        let reserved = v.reserved_assets();
        // accounting identities
        assert_eq!(v.total_assets(), liquid + deployed, "total_assets identity");
        assert!(liquid >= 0, "liquid never negative");
        assert_eq!(
            self.usdc.balance(&v.address),
            liquid + reserved,
            "vault USDC"
        );
        let claim_sum: i128 = self.users.iter().map(|u| v.claimable(u)).sum();
        assert_eq!(reserved, claim_sum, "reserved = sum claimable");
        assert_eq!(
            v.pending_shares(),
            v.balance(&v.address),
            "pending = custody"
        );
        let holders: i128 = self.users.iter().map(|u| v.balance(u)).sum::<i128>()
            + v.balance(&v.address)
            + v.balance(&self.owner);
        assert_eq!(v.total_supply(), holders, "supply = sum balances");
        // solvency: redeeming every share never needs more than total assets
        assert!(
            v.preview_redeem(&v.total_supply()) <= v.total_assets(),
            "solvency"
        );
        // conservation: assets = what came in, plus P&L, minus what went out
        // (claims still owed are outside total_assets)
        assert_eq!(
            v.total_assets() + reserved,
            self.net_in + self.pnl,
            "conservation"
        );
        // synchronous exits are closed while a queue exists
        if v.pending_shares() > 0 {
            for u in &self.users {
                assert_eq!(v.max_withdraw(u), 0, "queue closes sync exits");
            }
        }
        for u in &self.users {
            assert!(v.max_withdraw(u) <= liquid, "max_withdraw <= liquid");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_under_random_operations(ops in proptest::collection::vec(op(), 1..60)) {
        let mut w = world();
        w.check();
        for o in &ops {
            w.apply(o);
            w.check();
        }
        let _ = &w.e;
    }

    /// Whatever happened before, once capital is home every queued request
    /// can be processed and every holder can be paid.
    #[test]
    fn everyone_can_exit_once_capital_is_home(ops in proptest::collection::vec(op(), 1..40)) {
        let mut w = world();
        for o in &ops {
            w.apply(o);
        }
        let _ = w.v.try_unpause(&w.owner);
        let deployed = w.eng.total_value();
        if deployed > 0 {
            w.mint.mint(&w.v.address, &deployed);
            w.eng.set_value(&0);
        }
        w.v.process_redeems(&1_000);
        prop_assert_eq!(w.v.pending_shares(), 0);
        for i in 0..USERS {
            let s = w.v.max_redeem(&w.users[i]);
            if s > 0 {
                w.net_in -= w.v.redeem(&s, &w.users[i], &w.users[i], &w.users[i]);
            }
            if w.v.claimable(&w.users[i]) > 0 {
                w.net_in -= w.v.claim(&w.users[i]);
            }
            // dust that rounds to zero assets may remain
            prop_assert!(w.v.preview_redeem(&w.v.balance(&w.users[i])) == 0);
        }
        prop_assert_eq!(w.v.reserved_assets(), 0);
        w.check();
    }
}
