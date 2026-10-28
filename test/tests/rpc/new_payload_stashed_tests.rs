//! `engine_newPayload` for a payload stashed as ACCEPTED, and for its children.
//!
//! A payload whose parent is known but whose parent state is not reachable is stored
//! unexecuted and answered ACCEPTED, so a later forkchoiceUpdated can replay it. Storing
//! it put it on the known-block fast path, which answered VALID for anything it found
//! stored: delivering the payload again got VALID whatever it contained, and a child
//! built on whatever state root the payload claimed was executed on that state. Nothing
//! stored unexecuted may be answered VALID, and nothing may be executed on top of it.

use ethrex_rpc::test_utils::raw_params;
use std::{fs::File, io::BufReader, path::PathBuf};

use bytes::Bytes;
use ethrex_blockchain::{
    Blockchain, BlockchainOptions,
    payload::{BuildPayloadArgs, PayloadBuildResult, create_payload},
};
use ethrex_common::{
    H160, H256,
    types::{
        Block, BlockBody, BlockHeader, DEFAULT_BUILDER_GAS_CEIL, ELASTICITY_MULTIPLIER, Genesis,
    },
};
use ethrex_rpc::{
    engine::payload::NewPayloadV5Request, rpc::RpcHandler,
    test_utils::default_context_with_storage, types::payload::ExecutionPayload,
};
use ethrex_storage::{EngineType, Store};
use serde_json::{Value, json};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

async fn setup_store() -> Store {
    let file = File::open(workspace_root().join("fixtures/genesis/l1-bal.json"))
        .expect("open l1-bal genesis");
    let genesis: Genesis =
        serde_json::from_reader(BufReader::new(file)).expect("parse l1-bal genesis");
    let mut store = Store::new("store.db", EngineType::InMemory).expect("build in-memory store");
    store
        .add_initial_state(genesis)
        .await
        .expect("add genesis state");
    store
}

fn build_on(store: &Store, parent: &BlockHeader) -> PayloadBuildResult {
    let blockchain = Blockchain::new(store.clone(), BlockchainOptions::default());
    let args = BuildPayloadArgs {
        parent: parent.hash(),
        timestamp: parent.timestamp + 12,
        fee_recipient: H160::zero(),
        random: H256::zero(),
        withdrawals: Some(Vec::new()),
        beacon_root: Some(H256::zero()),
        slot_number: Some(1),
        version: 1,
        elasticity_multiplier: ELASTICITY_MULTIPLIER,
        gas_ceil: DEFAULT_BUILDER_GAS_CEIL,
    };
    let payload = create_payload(&args, store, Bytes::new()).unwrap();
    blockchain.build_payload(payload).unwrap()
}

fn new_payload_v5(block: Block, built: &PayloadBuildResult) -> NewPayloadV5Request {
    let payload = ExecutionPayload::from_block(block, built.block_access_list.clone());
    let requests: Vec<Value> = built
        .requests
        .iter()
        .filter(|r| r.0.len() > 1)
        .map(|r| serde_json::to_value(r).expect("request to json"))
        .collect();
    let params = Some(vec![
        serde_json::to_value(payload).expect("payload to json"),
        json!([]),
        json!(H256::zero()),
        Value::Array(requests),
    ]);
    NewPayloadV5Request::parse(&raw_params(&params)).expect("well-formed newPayloadV5 params")
}

/// A block built on genesis, re-parented onto `parent` at the next height, with its
/// state root replaced by `state_root`. Never executable, since nothing produced it.
fn reparented(built: &PayloadBuildResult, parent: &BlockHeader, state_root: H256) -> Block {
    let mut block = built.payload.clone();
    block.header.parent_hash = parent.hash();
    block.header.number = parent.number + 1;
    block.header.timestamp = parent.timestamp + 12;
    block.header.state_root = state_root;
    block.header.hash = Default::default();
    block
}

/// A header on genesis whose state root nothing materializes: a side block whose layer
/// was committed away, or any non-canonical block from before a restart.
async fn store_orphan_on_genesis(store: &Store, genesis: &BlockHeader) -> BlockHeader {
    let header = BlockHeader {
        number: 1,
        parent_hash: genesis.hash(),
        timestamp: genesis.timestamp + 12,
        state_root: H256::repeat_byte(0x77),
        hash: Default::default(),
        ..genesis.clone()
    };
    let block = Block::new(header.clone(), BlockBody::default());
    let header = block.header.clone();
    store.add_block(block).await.expect("store the orphan");
    header
}

/// A payload stashed as ACCEPTED is answered ACCEPTED again on re-delivery, not VALID:
/// it has still not been executed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redelivered_stashed_payload_is_not_valid() {
    let store = setup_store().await;
    let genesis = store.get_block_header(0).unwrap().unwrap();
    let orphan = store_orphan_on_genesis(&store, &genesis).await;
    let built = build_on(&store, &genesis);
    let stashed = reparented(&built, &orphan, H256::repeat_byte(0x99));
    let ctx = default_context_with_storage(store.clone()).await;
    let request = new_payload_v5(stashed.clone(), &built);

    let first = request.handle(ctx.clone()).await.expect("a payload status");
    assert_eq!(first["status"], "ACCEPTED", "got {first:?}");

    let again = request.handle(ctx.clone()).await.expect("a payload status");
    assert_eq!(again["status"], "ACCEPTED", "got {again:?}");
    assert!(
        store.get_bad_blocks().await.unwrap().is_empty(),
        "nothing was executed, so nothing can be known bad"
    );
}

/// A child of a stashed payload is stashed too, even when the state root the stashed
/// payload claims is one we hold (here, genesis'): executing the child on that state
/// would vouch for the whole unvalidated branch, or, as here, record the child as bad
/// for failing on a state that is not its parent's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_of_a_stashed_payload_is_stashed_not_executed() {
    let store = setup_store().await;
    let genesis = store.get_block_header(0).unwrap().unwrap();
    let orphan = store_orphan_on_genesis(&store, &genesis).await;
    let built = build_on(&store, &genesis);
    let stashed = reparented(&built, &orphan, genesis.state_root);
    let child = reparented(&built, &stashed.header, built.payload.header.state_root);
    let ctx = default_context_with_storage(store.clone()).await;

    let status = new_payload_v5(stashed.clone(), &built)
        .handle(ctx.clone())
        .await
        .expect("a payload status");
    assert_eq!(status["status"], "ACCEPTED", "got {status:?}");

    let status = new_payload_v5(child.clone(), &built)
        .handle(ctx.clone())
        .await
        .expect("a payload status");
    assert_eq!(status["status"], "ACCEPTED", "got {status:?}");
    assert!(
        store.get_bad_blocks().await.unwrap().is_empty(),
        "the child must not be executed on the state its parent merely claims"
    );
    assert_eq!(
        store.get_latest_valid_ancestor(child.hash()).await.unwrap(),
        None
    );
}

/// Once a stashed block is executed it is a validated block like any other, and is
/// answered VALID from then on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executed_stashed_block_is_valid() {
    let store = setup_store().await;
    let genesis = store.get_block_header(0).unwrap().unwrap();
    let built = build_on(&store, &genesis);
    let block = built.payload.clone();
    store
        .add_pending_block(block.clone())
        .expect("stash the block");
    assert!(store.has_pending_block(block.hash()).await.unwrap());

    let blockchain = Blockchain::new(store.clone(), BlockchainOptions::default());
    blockchain
        .add_block(block.clone())
        .expect("the block is valid");
    assert!(
        !store.has_pending_block(block.hash()).await.unwrap(),
        "executing a block clears its pending mark"
    );

    let ctx = default_context_with_storage(store.clone()).await;
    let status = new_payload_v5(block.clone(), &built)
        .handle(ctx.clone())
        .await
        .expect("a payload status");
    assert_eq!(status["status"], "VALID", "got {status:?}");
}
