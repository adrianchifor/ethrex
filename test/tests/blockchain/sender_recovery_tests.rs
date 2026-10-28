//! Sender recovery on the sequential path is bounded before it starts and ordered while
//! it runs.
//!
//! Every sender used to be recovered in parallel, and the whole list collected, before the
//! first transaction executed. Recovery is the one expensive thing done for a transaction
//! before anything about it is checked, so a block whose second transaction has a nonce
//! mismatch, followed by as many unexecutable transactions as its body may carry, cost the
//! recovery of every one of them (about 80 seconds for 620,000) before the nonce error was
//! reported. Now a sender-independent minimum-work bound rejects a block whose
//! transactions' gas floors cannot fit before any recovery, and the sequential loop
//! recovers senders in order, a chunk at a time, so a terminal error stops the rest.

use std::{fs::File, io::BufReader, path::PathBuf};

use bytes::Bytes;
use ethrex_blockchain::{
    Blockchain, BlockchainOptions,
    payload::{BuildPayloadArgs, create_payload},
};
use ethrex_common::{
    Address, H160, H256, U256,
    types::{
        Block, DEFAULT_BUILDER_GAS_CEIL, ELASTICITY_MULTIPLIER, Fork, Genesis, GenesisAccount,
        LegacyTransaction, Transaction, TxKind, compute_transactions_root,
    },
};
use ethrex_crypto::NativeCrypto;
use ethrex_l2_rpc::signer::{LocalSigner, Signable, Signer};
use ethrex_storage::{EngineType, Store};
use ethrex_vm::{block_work_budget, check_minimum_block_work_before_recovery};
use secp256k1::SecretKey;

const SINK: Address = H160([0x11; 20]);

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn key(index: u64) -> SecretKey {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&(index + 1).to_be_bytes());
    SecretKey::from_slice(&bytes).expect("small scalars are valid secret keys")
}

/// An Osaka store (the sequential path is the pre-Amsterdam one) with `funded` funded.
async fn setup_store(funded: &[Address]) -> Store {
    let file =
        File::open(workspace_root().join("fixtures/genesis/l1.json")).expect("open l1 genesis");
    let mut genesis: Genesis =
        serde_json::from_reader(BufReader::new(file)).expect("parse l1 genesis");
    for address in funded {
        genesis.alloc.insert(
            *address,
            GenesisAccount {
                code: Bytes::new(),
                storage: Default::default(),
                balance: U256::from(10u64).pow(U256::from(18u64)),
                nonce: 0,
            },
        );
    }
    let mut store = Store::new("store.db", EngineType::InMemory).expect("in-memory store");
    store.add_initial_state(genesis).await.expect("genesis");
    store
}

/// A valid empty block on genesis, whose header the tests reuse around their own
/// transaction lists.
fn empty_block(store: &Store) -> Block {
    let blockchain = Blockchain::new(store.clone(), BlockchainOptions::default());
    let genesis_header = store.get_block_header(0).unwrap().unwrap();
    let args = BuildPayloadArgs {
        parent: genesis_header.hash(),
        timestamp: genesis_header.timestamp + 12,
        fee_recipient: H160::zero(),
        random: H256::zero(),
        withdrawals: Some(Vec::new()),
        beacon_root: Some(H256::zero()),
        slot_number: None,
        version: 1,
        elasticity_multiplier: ELASTICITY_MULTIPLIER,
        gas_ceil: DEFAULT_BUILDER_GAS_CEIL,
    };
    let payload = create_payload(&args, store, Bytes::new()).unwrap();
    blockchain.build_payload(payload).unwrap().payload
}

fn legacy(nonce: u64, gas: u64, gas_price: u64) -> Transaction {
    Transaction::LegacyTransaction(LegacyTransaction {
        nonce,
        gas_price: U256::from(gas_price),
        gas,
        to: TxKind::Call(SINK),
        value: U256::zero(),
        data: Bytes::new(),
        ..Default::default()
    })
}

async fn signed(signer: &Signer, nonce: u64, gas: u64, gas_price: u64) -> Transaction {
    let mut tx = legacy(nonce, gas, gas_price);
    tx.sign_inplace(signer).await.unwrap();
    tx
}

/// A legacy transaction whose signature cannot be recovered: `r` is zero.
fn unrecoverable(nonce: u64, gas_price: u64) -> Transaction {
    let Transaction::LegacyTransaction(mut tx) = legacy(nonce, 21_000, gas_price) else {
        unreachable!()
    };
    tx.v = U256::from(27);
    tx.r = U256::zero();
    tx.s = U256::one();
    Transaction::LegacyTransaction(tx)
}

fn with_transactions(mut block: Block, transactions: Vec<Transaction>) -> Block {
    block.header.transactions_root = compute_transactions_root(&transactions, &NativeCrypto);
    block.header.hash = Default::default();
    block.body.transactions = transactions;
    block
}

fn import(store: Store, block: Block) -> Result<(), String> {
    Blockchain::new(store, BlockchainOptions::default())
        .add_block_pipeline(block, None)
        .map_err(|e| format!("{e:?}"))
}

/// A nonce mismatch at the second transaction is reported as such, with the unrecoverable
/// transactions after the recovery look-ahead never touched: recovering every sender up
/// front reported their bad signatures instead, after paying for all of them.
#[tokio::test]
async fn early_nonce_error_is_reported_before_the_unrecoverable_tail() {
    let signer: Signer = LocalSigner::new(key(0)).into();
    let store = setup_store(&[signer.address()]).await;
    let block = empty_block(&store);
    let base_fee = block.header.base_fee_per_gas.unwrap_or_default();

    let mut transactions = vec![
        signed(&signer, 0, 21_000, base_fee).await,
        // Nonce 5 at nonce 0: the block is invalid here.
        signed(&signer, 5, 21_000, base_fee).await,
    ];
    // Recoverable but unexecutable, more than one recovery chunk's worth.
    for index in 1..=300u64 {
        let other: Signer = LocalSigner::new(key(index)).into();
        transactions.push(signed(&other, 0, 21_000, base_fee).await);
    }
    for nonce in 0..20u64 {
        transactions.push(unrecoverable(nonce, base_fee));
    }

    let err = import(store, with_transactions(block, transactions))
        .expect_err("the block is invalid at its second transaction");
    assert!(
        !err.contains("recover"),
        "the unrecoverable tail must not be reached, got: {err}"
    );
    assert!(err.to_lowercase().contains("nonce"), "got: {err}");
}

/// A block of more zero-gas transactions than its gas limit can admit is rejected by the
/// minimum-work bound before any sender is recovered, not after recovering all of them.
#[tokio::test]
async fn block_of_unexecutable_transactions_is_rejected_before_recovery() {
    let signer: Signer = LocalSigner::new(key(0)).into();
    let store = setup_store(&[signer.address()]).await;
    let block = empty_block(&store);
    let base_fee = block.header.base_fee_per_gas.unwrap_or_default();
    // One more than the budget admits at the 21,000-gas floor.
    let count = block_work_budget(block.header.gas_limit) / 21_000 + 1;

    let mut transactions = Vec::with_capacity(count as usize);
    for nonce in 0..count {
        transactions.push(signed(&signer, nonce, 0, base_fee).await);
    }

    let err = import(store, with_transactions(block, transactions))
        .expect_err("the block cannot admit its transactions");
    assert!(
        err.contains("minimum gas of the block's transactions"),
        "got: {err}"
    );
}

/// The bound before recovery never rejects a block whose floors fit the budget, and
/// rejects one whose floors cannot, whatever the senders turn out to be.
#[test]
fn minimum_work_bound_before_recovery() {
    let gas_limit = 30_000_000;
    let fits: Vec<Transaction> = (0..100).map(|n| legacy(n, 0, 1)).collect();
    check_minimum_block_work_before_recovery(&fits, Fork::Osaka, gas_limit)
        .expect("100 floors fit");

    let count = block_work_budget(gas_limit) / 21_000 + 1;
    let too_many: Vec<Transaction> = (0..count).map(|n| legacy(n, 0, 1)).collect();
    let err = check_minimum_block_work_before_recovery(&too_many, Fork::Osaka, gas_limit)
        .expect_err("the floors exceed the budget");
    assert!(
        err.to_string()
            .contains("minimum gas of the block's transactions"),
        "got: {err}"
    );

    // Amsterdam: a self-transfer's floor is the EIP-2780 minimum, and the bound assumes
    // every call is one, so a block of calls to other accounts still passes when the
    // self-transfer floors fit.
    let calls: Vec<Transaction> = (0..1_000).map(|n| legacy(n, 0, 1)).collect();
    check_minimum_block_work_before_recovery(&calls, Fork::Amsterdam, gas_limit)
        .expect("1,000 self-transfer floors fit");
}
