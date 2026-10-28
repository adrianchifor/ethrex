//! Net-zero filtering at a transaction boundary costs the current transaction's writes,
//! not the block's write history.
//!
//! The recorder keeps every write to a slot for the whole block and filters the current
//! transaction's net-zero writes when the index advances. It used to scan a slot's whole
//! history for the current index and `retain` over it, so one slot with hundreds of
//! thousands of cheap writes early in the block made every later net-zero transaction on
//! it cost that history again: a 200M-gas block took 25 seconds to reject instead of 1.5.

use std::time::{Duration, Instant};

use ethrex_common::{Address, U256, types::block_access_list::BlockAccessListRecorder};

const ALICE: Address = Address::repeat_byte(0xaa);
const SLOT: U256 = U256([7, 0, 0, 0]);

/// Writes `history` entries to `SLOT` in transaction 1 that end away from the pre-value,
/// then runs `net_zero` transactions that each write the slot and write it back.
fn record(history: usize, net_zero: u32) -> BlockAccessListRecorder {
    let mut recorder = BlockAccessListRecorder::new();
    recorder.set_block_access_index(1);
    recorder.capture_pre_storage(ALICE, SLOT, U256::one());
    for i in 0..history {
        recorder.record_storage_write(ALICE, SLOT, U256::from(2 + (i % 2) as u64));
    }
    for tx in 0..net_zero {
        recorder.set_block_access_index(2 + tx);
        // The pre-value is whatever the previous transaction left: 2 or 3.
        let pre = U256::from(2 + (history.saturating_sub(1) % 2) as u64);
        recorder.capture_pre_storage(ALICE, SLOT, pre);
        recorder.record_storage_write(ALICE, SLOT, U256::from(9));
        recorder.record_storage_write(ALICE, SLOT, pre);
    }
    recorder
}

/// The net-zero transactions are filtered and the earlier history kept, whatever its
/// length.
#[test]
fn net_zero_transactions_are_filtered_and_history_kept() {
    let bal = record(5, 3).build();
    let account = &bal.accounts()[0];
    assert_eq!(account.storage_changes.len(), 1);
    let changes = &account.storage_changes[0].slot_changes;
    assert_eq!(changes.len(), 1, "one entry per index, got {changes:?}");
    assert_eq!(changes[0].block_access_index, 1);
    assert_eq!(changes[0].post_value, U256::from(2));

    // With no earlier write the net-zero transactions leave only a read.
    let bal = record(0, 3).build();
    let account = &bal.accounts()[0];
    assert!(account.storage_changes.is_empty());
    assert!(account.storage_reads.contains(&SLOT));
}

/// Twenty thousand net-zero boundaries after a million-entry history finish in well under
/// a second; scanning the history at every boundary took seconds.
#[test]
fn net_zero_cleanup_does_not_scan_the_history() {
    let start = Instant::now();
    let bal = record(1_000_000, 20_000).build();
    let elapsed = start.elapsed();
    assert_eq!(bal.accounts()[0].storage_changes[0].slot_changes.len(), 1);
    assert!(
        elapsed < Duration::from_secs(2),
        "net-zero cleanup took {elapsed:?}; it must not grow with the slot's history"
    );
}
