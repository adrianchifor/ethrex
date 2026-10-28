//! The speculative warmer's sender grouping on an attacker-shaped transaction set.
//!
//! The warmer groups a block's transactions by sender before executing the groups in
//! parallel. The map was keyed by a fixed hash, and EIP-8141 Frame transactions name
//! their sender outright, so a payload could choose tens of thousands of senders with
//! one hash value and turn the grouping quadratic, with no cancellation check until the
//! groups ran: one invalid payload of 40,000 such transactions held the importer for 17
//! seconds although its first transaction already rejected the block.

use std::{
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use ethrex_common::{
    Address, U256,
    types::{BlockHeader, LegacyTransaction, Transaction, TxKind},
};
use ethrex_crypto::NativeCrypto;
use ethrex_vm::backends::levm::LEVM;
use rustc_hash::FxHasher;

use super::test_db::TestDatabase;

/// The middle word that makes the fixed hash of a 20-byte address collapse: its byte
/// hashing xors the second seed into this word before the final mix, so these addresses
/// all hash alike whatever their last eight bytes.
const FXHASH_SEED2: u64 = 0x13198a2e03707344;

fn colliding_sender(index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[4..12].copy_from_slice(&FXHASH_SEED2.to_le_bytes());
    bytes[12..20].copy_from_slice(&index.to_le_bytes());
    Address::from_slice(&bytes)
}

fn fx_hash(address: &Address) -> u64 {
    let mut hasher = FxHasher::default();
    address.hash(&mut hasher);
    hasher.finish()
}

fn transaction() -> Transaction {
    Transaction::LegacyTransaction(LegacyTransaction {
        gas: 21_000,
        to: TxKind::Call(Address::repeat_byte(0x11)),
        value: U256::zero(),
        data: Bytes::new(),
        ..Default::default()
    })
}

/// Runs the warmer over `count` transactions whose senders all share one fixed hash and
/// returns how long it took; `should_stop` is what the executor's cancellation looks
/// like to it.
fn warm_colliding(count: usize, should_stop: &(dyn Fn() -> bool + Sync)) -> Duration {
    let senders: Vec<Address> = (0..count as u64).map(colliding_sender).collect();
    assert!(
        senders[1..4]
            .iter()
            .all(|s| fx_hash(s) == fx_hash(&senders[0])),
        "the senders must collide under the fixed hash for the test to mean anything"
    );
    let tx = transaction();
    let txs_with_sender: Vec<(&Transaction, Address)> =
        senders.iter().map(|sender| (&tx, *sender)).collect();
    let store: Arc<dyn ethrex_levm::db::Database> = Arc::new(TestDatabase::new());

    let start = Instant::now();
    LEVM::warm_txs(
        &txs_with_sender,
        &BlockHeader::default(),
        store,
        ethrex_levm::vm::VMType::L1,
        &NativeCrypto,
        should_stop,
    )
    .expect("warming is best-effort and returns Ok");
    start.elapsed()
}

const SENDERS: usize = 100_000;

/// Grouping tens of thousands of colliding senders costs about as much as grouping
/// distinct ones: the stop flag is first raised once grouping is done, so the time is
/// the grouping's.
#[test]
fn colliding_senders_do_not_make_grouping_quadratic() {
    let polls = AtomicUsize::new(0);
    let should_stop = || polls.fetch_add(1, Ordering::Relaxed) >= SENDERS;
    let elapsed = warm_colliding(SENDERS, &should_stop);
    assert!(
        elapsed < Duration::from_secs(1),
        "grouping {SENDERS} colliding senders took {elapsed:?}"
    );
}

/// A warmer told to stop before it starts does no grouping work at all.
#[test]
fn cancellation_is_honored_while_grouping() {
    let elapsed = warm_colliding(SENDERS, &|| true);
    assert!(
        elapsed < Duration::from_millis(500),
        "a cancelled warmer still grouped {SENDERS} senders: {elapsed:?}"
    );
}
