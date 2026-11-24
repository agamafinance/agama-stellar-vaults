extern crate std;

use agama_allocation_engine::{AllocationEngine, AllocationEngineClient};
use agama_etherfuse_adapter::{Config as EfConfig, EtherfuseAdapter, EtherfuseAdapterClient};
use agama_interfaces::Asset;
use agama_oracle::{AgamaOracle, AgamaOracleClient};
use agama_private_credit_adapter::{
    Config as PcConfig, PrivateCreditAdapter, PrivateCreditAdapterClient,
};
use agama_vault::{AgamaVaultContract, AgamaVaultContractClient};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env, String,
};

const USDC: i128 = 10_000_000;
const ONE: i128 = 10_000_000;
const TK: i128 = 1_000_000;
const DAY: u64 = 86_400;

struct System<'a> {
    e: Env,
    owner: Address,
    operator: Address,
    reporter: Address,
    usdc: TokenClient<'a>,
    mint: StellarAssetClient<'a>,
    vault: AgamaVaultContractClient<'a>,
    engine: AllocationEngineClient<'a>,
    oracle: AgamaOracleClient<'a>,
    bond: Address,
    mint_bond: StellarAssetClient<'a>,
    tenka: PrivateCreditAdapterClient<'a>,
    bonds: EtherfuseAdapterClient<'a>,
}

fn tkusd() -> Asset {
    Asset::Other(symbol_short!("TKUSD"))
}

fn deploy<'a>() -> System<'a> {
    let e = Env::default();
    e.mock_all_auths();
    e.ledger().set_timestamp(100 * DAY);
    let owner = Address::generate(&e);
    let curator = Address::generate(&e);
    let operator = Address::generate(&e);
    let reporter = Address::generate(&e);
    let usdc_id = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();

    let vault_id = e.register(
        AgamaVaultContract,
        (
            owner.clone(),
            usdc_id.clone(),
            String::from_str(&e, "Agama USD"),
            String::from_str(&e, "agUSD"),
        ),
    );
    let engine_id = e.register(
        AllocationEngine,
        (owner.clone(), curator, vault_id.clone(), usdc_id.clone()),
    );
    let oracle_id = e.register(
        AgamaOracle,
        (
            owner.clone(),
            Asset::Stellar(usdc_id.clone()),
            7u32,
            DAY as u32,
        ),
    );
    let bond_id = e
        .register_stellar_asset_contract_v2(owner.clone())
        .address();

    let oracle = AgamaOracleClient::new(&e, &oracle_id);
    oracle.set_reporter(&reporter, &true);
    oracle.add_feed(&tkusd(), &500);
    oracle.add_feed(&Asset::Stellar(bond_id.clone()), &0);
    oracle.set_price(&reporter, &tkusd(), &ONE, &(100 * DAY));
    oracle.set_price(
        &reporter,
        &Asset::Stellar(bond_id.clone()),
        &ONE,
        &(100 * DAY),
    );

    let tenka_id = e.register(
        PrivateCreditAdapter,
        (
            owner.clone(),
            PcConfig {
                engine: engine_id.clone(),
                asset: usdc_id.clone(),
                operator: operator.clone(),
                oracle: oracle_id.clone(),
                feed: tkusd(),
                unit_decimals: 6,
                max_age: 14 * DAY,
                max_slippage_bps: 100,
            },
        ),
    );
    let bonds_id = e.register(
        EtherfuseAdapter,
        (
            owner.clone(),
            EfConfig {
                engine: engine_id.clone(),
                asset: usdc_id.clone(),
                bond: bond_id.clone(),
                operator: operator.clone(),
                oracle: oracle_id.clone(),
                feed: Asset::Stellar(bond_id.clone()),
                max_age: 14 * DAY,
                max_slippage_bps: 50,
            },
        ),
    );

    let vault = AgamaVaultContractClient::new(&e, &vault_id);
    vault.set_engine(&engine_id);
    let engine = AllocationEngineClient::new(&e, &engine_id);
    engine.register_adapter(
        &tenka_id,
        &symbol_short!("TENKA"),
        &symbol_short!("LU"),
        &7_000,
    );
    engine.register_adapter(
        &bonds_id,
        &symbol_short!("EFUSE"),
        &symbol_short!("MX"),
        &5_000,
    );
    engine.set_reserve_floor(&1_000);

    System {
        owner,
        operator,
        reporter,
        usdc: TokenClient::new(&e, &usdc_id),
        mint: StellarAssetClient::new(&e, &usdc_id),
        vault,
        engine,
        oracle,
        mint_bond: StellarAssetClient::new(&e, &bond_id),
        bond: bond_id,
        tenka: PrivateCreditAdapterClient::new(&e, &tenka_id),
        bonds: EtherfuseAdapterClient::new(&e, &bonds_id),
        e,
    }
}

impl System<'_> {
    fn user(&self, usdc: i128) -> Address {
        let u = Address::generate(&self.e);
        self.mint.mint(&u, &usdc);
        u
    }
    fn at(&self, t: u64) {
        self.e.ledger().set_timestamp(t);
    }
}

#[test]
fn deposit_allocate_earn_queue_and_exit() {
    let s = deploy();

    // 1. Deposits: agUSD minted 1:1 on an empty vault.
    let alice = s.user(600 * USDC);
    let bob = s.user(400 * USDC);
    s.vault.deposit(&(600 * USDC), &alice, &alice, &alice);
    s.vault.deposit(&(400 * USDC), &bob, &bob, &bob);
    assert_eq!(s.vault.total_assets(), 1_000 * USDC);

    // 2. Curator allocates: 600 to Tenka, 300 to bonds, 100 stays (floor 10%).
    s.engine.allocate(&s.tenka.address, &(600 * USDC));
    s.engine.allocate(&s.bonds.address, &(300 * USDC));
    assert!(s.engine.try_allocate(&s.bonds.address, &1).is_err()); // floor
    assert_eq!(s.vault.liquid_assets(), 100 * USDC);
    assert_eq!(s.vault.total_assets(), 1_000 * USDC);

    // 3a. Operator buys Stablebonds through Etherfuse and delivers them.
    s.bonds.dispatch(&(300 * USDC));
    s.mint_bond.mint(&s.operator, &(300 * ONE));
    s.bonds.confirm_purchase(&(300 * USDC), &(300 * ONE));
    assert_eq!(s.vault.total_assets(), 1_000 * USDC);

    // 3b. Operator bridges to Tenka and books the tkUSD units.
    s.tenka.dispatch(&(600 * USDC));
    s.tenka.confirm_subscription(&(600 * USDC), &(600 * TK));
    assert_eq!(s.vault.total_assets(), 1_000 * USDC);

    // 4. A week passes: tkUSD +0.3%, bonds +0.1%.
    s.at(107 * DAY);
    s.oracle
        .set_price(&s.reporter, &tkusd(), &(ONE * 1003 / 1000), &(107 * DAY));
    s.oracle.set_price(
        &s.reporter,
        &Asset::Stellar(s.bond.clone()),
        &(ONE * 1001 / 1000),
        &(107 * DAY),
    );
    let expected = 100 * USDC + 600 * USDC * 1003 / 1000 + 300 * USDC * 1001 / 1000;
    assert_eq!(s.vault.total_assets(), expected);
    let alice_value = s.vault.preview_redeem(&s.vault.balance(&alice));
    assert!(alice_value > 601 * USDC);

    // 5. Alice wants out entirely: more than the vault holds, so she queues.
    assert!(s.vault.max_redeem(&alice) < s.vault.balance(&alice));
    let shares = s.vault.balance(&alice);
    s.vault.request_redeem(&shares, &alice);
    assert_eq!(s.vault.max_withdraw(&bob), 0); // queue closes sync exits
    assert_eq!(s.vault.process_redeems(&5), 0); // not enough liquidity

    // 6. The operator sells the bonds and Tenka redeems part; the curator
    //    brings the USDC home.
    s.bonds.dispatch_bonds(&(300 * ONE));
    let proceeds = 300 * USDC * 1001 / 1000;
    s.mint.mint(&s.operator, &proceeds);
    s.bonds.settle_sale(&(300 * ONE), &proceeds);
    s.engine.deallocate(&s.bonds.address, &proceeds);
    let units = 300 * TK;
    let back = 300 * USDC * 1003 / 1000;
    s.mint.mint(&s.operator, &back); // USDC bridged back from Ethereum
    s.tenka.settle_redemption(&units, &back);
    s.engine.deallocate(&s.tenka.address, &back);

    // 7. The queue is processed and Alice claims.
    assert_eq!(s.vault.process_redeems(&5), 1);
    let owed = s.vault.claimable(&alice);
    assert!(owed > 601 * USDC && owed < 602 * USDC);
    s.vault.claim(&alice);
    assert_eq!(s.usdc.balance(&alice), owed);

    // 8. Bob's position kept its share of the yield.
    let bob_value = s.vault.preview_redeem(&s.vault.balance(&bob));
    assert!(bob_value > 400 * USDC && bob_value < 402 * USDC);
    assert_eq!(s.vault.pending_shares(), 0);
    assert!(s.vault.max_withdraw(&bob) > 0);
}

#[test]
fn stale_credit_price_freezes_the_vault_until_refreshed() {
    let s = deploy();
    let alice = s.user(100 * USDC);
    s.vault.deposit(&(100 * USDC), &alice, &alice, &alice);
    s.engine.allocate(&s.tenka.address, &(50 * USDC));
    s.tenka.dispatch(&(50 * USDC));
    s.tenka.confirm_subscription(&(50 * USDC), &(50 * TK));

    s.at(100 * DAY + 15 * DAY);
    let bob = s.user(10 * USDC);
    assert!(s.vault.try_deposit(&(10 * USDC), &bob, &bob, &bob).is_err());
    assert!(s.vault.try_total_assets().is_err());

    s.oracle
        .set_price(&s.reporter, &tkusd(), &ONE, &(115 * DAY));
    s.oracle.set_price(
        &s.reporter,
        &Asset::Stellar(s.bond.clone()),
        &ONE,
        &(115 * DAY),
    );
    s.vault.deposit(&(10 * USDC), &bob, &bob, &bob);
    assert_eq!(s.vault.total_assets(), 110 * USDC);
}

#[test]
fn credit_write_down_hits_every_holder_pro_rata() {
    let s = deploy();
    let alice = s.user(100 * USDC);
    s.vault.deposit(&(100 * USDC), &alice, &alice, &alice);
    s.engine.allocate(&s.tenka.address, &(50 * USDC));
    s.tenka.dispatch(&(50 * USDC));
    s.tenka.confirm_subscription(&(50 * USDC), &(50 * TK));

    s.at(101 * DAY);
    s.oracle.force_price(&tkusd(), &(ONE / 2), &(101 * DAY));
    assert_eq!(s.vault.total_assets(), 75 * USDC);
    let _ = &s.owner;
    assert_eq!(s.vault.preview_redeem(&(100 * USDC)), 75 * USDC);
}
