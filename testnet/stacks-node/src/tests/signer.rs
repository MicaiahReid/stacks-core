use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use clarity::boot_util::boot_code_id;
use hashbrown::HashMap;
use libsigner::{
    BlockResponse, RejectCode, RunningSigner, Signer, SignerEventReceiver, SignerMessage,
    BLOCK_MSG_ID, TRANSACTIONS_MSG_ID,
};
use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::chainstate::nakamoto::signer_set::NakamotoSigners;
use stacks::chainstate::nakamoto::{NakamotoBlock, NakamotoBlockHeader};
use stacks::chainstate::stacks::boot::SIGNERS_NAME;
use stacks::chainstate::stacks::{
    StacksPrivateKey, StacksTransaction, ThresholdSignature, TransactionAnchorMode,
    TransactionAuth, TransactionPayload, TransactionPostConditionMode, TransactionSmartContract,
    TransactionVersion,
};
use stacks::net::api::postblock_proposal::BlockValidateResponse;
use stacks::util_lib::strings::StacksString;
use stacks_common::bitvec::BitVec;
use stacks_common::codec::read_next;
use stacks_common::consts::SIGNER_SLOTS_PER_USER;
use stacks_common::types::chainstate::{
    ConsensusHash, StacksAddress, StacksBlockId, StacksPublicKey, TrieHash,
};
use stacks_common::util::hash::{MerkleTree, Sha512Trunc256Sum};
use stacks_common::util::secp256k1::MessageSignature;
use stacks_signer::client::{StackerDB, StacksClient};
use stacks_signer::config::{build_signer_config_tomls, GlobalConfig as SignerConfig, Network};
use stacks_signer::runloop::RunLoopCommand;
use stacks_signer::signer::Command as SignerCommand;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};
use wsts::curve::point::Point;
use wsts::state_machine::OperationResult;

use crate::config::{Config as NeonConfig, EventKeyType, EventObserverConfig, InitialBalance};
use crate::neon::Counters;
use crate::run_loop::boot_nakamoto;
use crate::tests::bitcoin_regtest::BitcoinCoreController;
use crate::tests::nakamoto_integrations::{
    boot_to_epoch_3, naka_neon_integration_conf, next_block_and, next_block_and_mine_commit,
    POX_4_DEFAULT_STACKER_BALANCE,
};
use crate::tests::neon_integrations::{next_block_and_wait, test_observer, wait_for_runloop};
use crate::tests::to_addr;
use crate::{BitcoinRegtestController, BurnchainController};

// Helper struct for holding the btc and stx neon nodes
#[allow(dead_code)]
struct RunningNodes {
    pub btc_regtest_controller: BitcoinRegtestController,
    pub btcd_controller: BitcoinCoreController,
    pub run_loop_thread: thread::JoinHandle<()>,
    pub run_loop_stopper: Arc<AtomicBool>,
    pub vrfs_submitted: Arc<AtomicU64>,
    pub commits_submitted: Arc<AtomicU64>,
    pub blocks_processed: Arc<AtomicU64>,
    pub coord_channel: Arc<Mutex<CoordinatorChannels>>,
    pub conf: NeonConfig,
}

struct SignerTest {
    // The stx and bitcoin nodes and their run loops
    pub running_nodes: RunningNodes,
    // The channels for sending commands to the signers
    pub signer_cmd_senders: HashMap<u32, Sender<RunLoopCommand>>,
    // The channels for receiving results from the signers
    pub result_receivers: Vec<Receiver<Vec<OperationResult>>>,
    // The running signer and its threads
    pub running_signers: HashMap<u32, RunningSigner<SignerEventReceiver, Vec<OperationResult>>>,
    // the private keys of the signers
    pub signer_stacks_private_keys: Vec<StacksPrivateKey>,
    // link to the stacks node
    pub stacks_client: StacksClient,
}

impl SignerTest {
    fn new(num_signers: u32, _num_keys: u32) -> Self {
        // Generate Signer Data
        let signer_stacks_private_keys = (0..num_signers)
            .map(|_| StacksPrivateKey::new())
            .collect::<Vec<StacksPrivateKey>>();

        let (naka_conf, _miner_account) = naka_neon_integration_conf(None);

        // Setup the signer and coordinator configurations
        let signer_configs = build_signer_config_tomls(
            &signer_stacks_private_keys,
            &naka_conf.node.rpc_bind,
            Some(Duration::from_millis(128)), // Timeout defaults to 5 seconds. Let's override it to 128 milliseconds.
            &Network::Testnet,
        );

        let mut running_signers = HashMap::new();
        let mut signer_cmd_senders = HashMap::new();
        let mut result_receivers = Vec::new();
        // Spawn all signers before the node to ensure their listening ports are open for the node event observer to bind to
        for i in (0..num_signers).rev() {
            let (cmd_send, cmd_recv) = channel();
            let (res_send, res_recv) = channel();
            info!("spawn signer");
            running_signers.insert(
                i,
                spawn_signer(&signer_configs[i as usize], cmd_recv, res_send),
            );
            signer_cmd_senders.insert(i, cmd_send);
            result_receivers.push(res_recv);
        }

        // Setup the nodes and deploy the contract to it
        let node = setup_stx_btc_node(
            naka_conf,
            num_signers,
            &signer_stacks_private_keys,
            &signer_configs,
        );
        let config = SignerConfig::load_from_str(&signer_configs[0]).unwrap();
        let stacks_client = StacksClient::from(&config);

        Self {
            running_nodes: node,
            result_receivers,
            signer_cmd_senders,
            running_signers,
            signer_stacks_private_keys,
            stacks_client,
        }
    }

    fn shutdown(self) {
        self.running_nodes
            .coord_channel
            .lock()
            .expect("Mutex poisoned")
            .stop_chains_coordinator();

        self.running_nodes
            .run_loop_stopper
            .store(false, Ordering::SeqCst);

        self.running_nodes.run_loop_thread.join().unwrap();
        // Stop the signers
        for (_id, signer) in self.running_signers {
            assert!(signer.stop().is_none());
        }
    }
}

fn spawn_signer(
    data: &str,
    receiver: Receiver<RunLoopCommand>,
    sender: Sender<Vec<OperationResult>>,
) -> RunningSigner<SignerEventReceiver, Vec<OperationResult>> {
    let config = SignerConfig::load_from_str(data).unwrap();
    let ev = SignerEventReceiver::new(config.network.is_mainnet());
    let endpoint = config.endpoint;
    let runloop: stacks_signer::runloop::RunLoop = stacks_signer::runloop::RunLoop::from(config);
    let mut signer: Signer<
        RunLoopCommand,
        Vec<OperationResult>,
        stacks_signer::runloop::RunLoop,
        SignerEventReceiver,
    > = Signer::new(runloop, ev, receiver, sender);
    info!("Spawning signer on endpoint {}", endpoint);
    signer.spawn(endpoint).unwrap()
}

fn setup_stx_btc_node(
    mut naka_conf: NeonConfig,
    num_signers: u32,
    signer_stacks_private_keys: &[StacksPrivateKey],
    signer_config_tomls: &Vec<String>,
) -> RunningNodes {
    // Spawn the endpoints for observing signers
    for toml in signer_config_tomls {
        let signer_config = SignerConfig::load_from_str(toml).unwrap();

        naka_conf.events_observers.insert(EventObserverConfig {
            endpoint: format!("{}", signer_config.endpoint),
            events_keys: vec![EventKeyType::StackerDBChunks, EventKeyType::BlockProposal],
        });
    }

    // Spawn a test observer for verification purposes
    test_observer::spawn();
    let observer_port = test_observer::EVENT_OBSERVER_PORT;
    naka_conf.events_observers.insert(EventObserverConfig {
        endpoint: format!("localhost:{observer_port}"),
        events_keys: vec![EventKeyType::StackerDBChunks, EventKeyType::BlockProposal],
    });

    // The signers need some initial balances in order to pay for epoch 2.5 transaction votes
    let mut initial_balances = Vec::new();

    // TODO: separate keys for stacking and signing (because they'll be different in prod)
    for i in 0..num_signers {
        initial_balances.push(InitialBalance {
            address: to_addr(&signer_stacks_private_keys[i as usize]).into(),
            amount: POX_4_DEFAULT_STACKER_BALANCE,
        });
    }
    naka_conf.initial_balances.append(&mut initial_balances);
    naka_conf.node.stacker = true;
    naka_conf.miner.wait_on_interim_blocks = Duration::from_secs(1000);

    for signer_set in 0..2 {
        for message_id in 0..SIGNER_SLOTS_PER_USER {
            let contract_id =
                NakamotoSigners::make_signers_db_contract_id(signer_set, message_id, false);
            if !naka_conf.node.stacker_dbs.contains(&contract_id) {
                debug!("A miner/stacker must subscribe to the {contract_id} stacker db contract. Forcibly subscribing...");
                naka_conf.node.stacker_dbs.push(contract_id);
            }
        }
    }

    info!("Make new BitcoinCoreController");
    let mut btcd_controller = BitcoinCoreController::new(naka_conf.clone());
    btcd_controller
        .start_bitcoind()
        .map_err(|_e| ())
        .expect("Failed starting bitcoind");

    info!("Make new BitcoinRegtestController");
    let mut btc_regtest_controller = BitcoinRegtestController::new(naka_conf.clone(), None);

    info!("Bootstraping...");
    btc_regtest_controller.bootstrap_chain(201);

    info!("Chain bootstrapped...");

    let mut run_loop = boot_nakamoto::BootRunLoop::new(naka_conf.clone()).unwrap();
    let run_loop_stopper = run_loop.get_termination_switch();
    let Counters {
        blocks_processed,
        naka_submitted_vrfs: vrfs_submitted,
        naka_submitted_commits: commits_submitted,
        ..
    } = run_loop.counters();

    let coord_channel = run_loop.coordinator_channels();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Give the run loop some time to start up!
    info!("Wait for runloop...");
    wait_for_runloop(&blocks_processed);

    // First block wakes up the run loop.
    info!("Mine first block...");
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // Second block will hold our VRF registration.
    info!("Mine second block...");
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // Third block will be the first mined Stacks block.
    info!("Mine third block...");
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    info!("Boot to epoch 3.0 to activate pox-4...");
    boot_to_epoch_3(
        &naka_conf,
        &blocks_processed,
        signer_stacks_private_keys,
        signer_stacks_private_keys,
        &mut btc_regtest_controller,
    );

    info!("Pox 4 activated and ready for signers to perform DKG and Sign!");
    RunningNodes {
        btcd_controller,
        btc_regtest_controller,
        run_loop_thread,
        run_loop_stopper,
        vrfs_submitted,
        commits_submitted,
        blocks_processed,
        coord_channel,
        conf: naka_conf,
    }
}

#[test]
#[ignore]
/// Test the signer can respond to external commands to perform DKG
/// and sign a block with both taproot and non-taproot signatures
fn stackerdb_dkg_sign() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    info!("------------------------- Test Setup -------------------------");

    info!("Creating an invalid block to sign...");
    let header = NakamotoBlockHeader {
        version: 1,
        chain_length: 2,
        burn_spent: 3,
        consensus_hash: ConsensusHash([0x04; 20]),
        parent_block_id: StacksBlockId([0x05; 32]),
        tx_merkle_root: Sha512Trunc256Sum([0x06; 32]),
        state_index_root: TrieHash([0x07; 32]),
        miner_signature: MessageSignature::empty(),
        signer_signature: ThresholdSignature::empty(),
        signer_bitvec: BitVec::zeros(1).unwrap(),
    };
    let mut block = NakamotoBlock {
        header,
        txs: vec![],
    };
    let tx_merkle_root = {
        let txid_vecs = block
            .txs
            .iter()
            .map(|tx| tx.txid().as_bytes().to_vec())
            .collect();

        MerkleTree::<Sha512Trunc256Sum>::new(&txid_vecs).root()
    };
    block.header.tx_merkle_root = tx_merkle_root;

    // The block is invalid so the signers should return a signature across its hash + b'n'
    let mut msg = block.header.signer_signature_hash().0.to_vec();
    msg.push(b'n');

    let signer_test = SignerTest::new(10, 400);

    info!("------------------------- Test DKG -------------------------");
    info!("signer_runloop: spawn send commands to do dkg");
    let dkg_now = Instant::now();
    let dkg_command = RunLoopCommand {
        reward_cycle: 0,
        command: SignerCommand::Dkg,
    };
    for cmd_sender in signer_test.signer_cmd_senders.values() {
        cmd_sender
            .send(dkg_command.clone())
            .expect("failed to send Dkg command");
    }
    let mut key = Point::default();
    for recv in signer_test.result_receivers.iter() {
        let mut aggregate_public_key = None;
        loop {
            let results = recv
                .recv_timeout(Duration::from_secs(100))
                .expect("failed to recv dkg results");
            for result in results {
                match result {
                    OperationResult::Sign(sig) => {
                        panic!("Received Signature ({},{})", &sig.R, &sig.z);
                    }
                    OperationResult::SignTaproot(proof) => {
                        panic!("Received SchnorrProof ({},{})", &proof.r, &proof.s);
                    }
                    OperationResult::DkgError(dkg_error) => {
                        panic!("Received DkgError {:?}", dkg_error);
                    }
                    OperationResult::SignError(sign_error) => {
                        panic!("Received SignError {}", sign_error);
                    }
                    OperationResult::Dkg(point) => {
                        info!("Received aggregate_group_key {point}");
                        aggregate_public_key = Some(point);
                    }
                }
            }
            if aggregate_public_key.is_some() || dkg_now.elapsed() > Duration::from_secs(100) {
                break;
            }
        }
        key = aggregate_public_key.expect("Failed to get aggregate public key within 100 seconds");
    }
    let dkg_elapsed = dkg_now.elapsed();

    info!("------------------------- Test Sign -------------------------");
    let sign_now = Instant::now();
    info!("signer_runloop: spawn send commands to do dkg and then sign");
    let sign_command = RunLoopCommand {
        reward_cycle: 0,
        command: SignerCommand::Sign {
            block: block.clone(),
            is_taproot: false,
            merkle_root: None,
        },
    };
    let sign_taproot_command = RunLoopCommand {
        reward_cycle: 0,
        command: SignerCommand::Sign {
            block: block.clone(),
            is_taproot: true,
            merkle_root: None,
        },
    };
    for cmd_sender in signer_test.signer_cmd_senders.values() {
        cmd_sender
            .send(sign_command.clone())
            .expect("failed to send non taproot Sign command");
        cmd_sender
            .send(sign_taproot_command.clone())
            .expect("failed to send taproot Sign command");
    }
    for recv in signer_test.result_receivers.iter() {
        let mut frost_signature = None;
        let mut schnorr_proof = None;
        loop {
            let results = recv
                .recv_timeout(Duration::from_secs(30))
                .expect("failed to recv signature results");
            for result in results {
                match result {
                    OperationResult::Sign(sig) => {
                        info!("Received Signature ({},{})", &sig.R, &sig.z);
                        frost_signature = Some(sig);
                    }
                    OperationResult::SignTaproot(proof) => {
                        info!("Received SchnorrProof ({},{})", &proof.r, &proof.s);
                        schnorr_proof = Some(proof);
                    }
                    OperationResult::DkgError(dkg_error) => {
                        panic!("Received DkgError {:?}", dkg_error);
                    }
                    OperationResult::SignError(sign_error) => {
                        panic!("Received SignError {}", sign_error);
                    }
                    OperationResult::Dkg(point) => {
                        panic!("Received aggregate_group_key {point}");
                    }
                }
            }
            if (frost_signature.is_some() && schnorr_proof.is_some())
                || sign_now.elapsed() > Duration::from_secs(100)
            {
                break;
            }
        }
        let frost_signature =
            frost_signature.expect("Failed to get frost signature within 100 seconds");
        assert!(
            frost_signature.verify(&key, msg.as_slice()),
            "Signature verification failed"
        );
        let schnorr_proof =
            schnorr_proof.expect("Failed to get schnorr proof signature within 100 seconds");
        let tweaked_key = wsts::compute::tweaked_public_key(&key, None);
        assert!(
            schnorr_proof.verify(&tweaked_key.x(), &msg.as_slice()),
            "Schnorr proof verification failed"
        );
    }
    let sign_elapsed = sign_now.elapsed();

    info!("DKG Time Elapsed: {:.2?}", dkg_elapsed);
    info!("Sign Time Elapsed: {:.2?}", sign_elapsed);
    signer_test.shutdown();
}

#[test]
#[ignore]
/// Test that a signer can respond to a miners request for a signature on a block proposal
///
/// Test Setup:
/// The test spins up five stacks signers, one miner Nakamoto node, and a corresponding bitcoind.
/// The stacks node is advanced to epoch 3.0, triggering signers to perform DKG round.
///
/// Test Execution:
/// The node attempts to mine a Nakamoto tenure, sending a block to the observing signers via the
/// .miners stacker db instance. The signers submit the block to the stacks node for verification.
/// Upon receiving a Block Validation response approving the block, the signers perform a signing
/// round across its signature hash.
///
/// Test Assertion:
/// Signers return an operation result containing a valid signature across the miner's Nakamoto block's signature hash.
/// Signers broadcasted a signature across the miner's proposed block back to the respective .signers-XXX-YYY contract.
/// TODO: update test to check miner received the signed block and appended it to the chain
fn stackerdb_block_proposal() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    info!("------------------------- Test Setup -------------------------");
    let mut signer_test = SignerTest::new(5, 5);

    let (vrfs_submitted, commits_submitted) = (
        signer_test.running_nodes.vrfs_submitted.clone(),
        signer_test.running_nodes.commits_submitted.clone(),
    );

    info!("Mining a Nakamoto tenure...");

    // first block wakes up the run loop, wait until a key registration has been submitted.
    next_block_and(
        &mut signer_test.running_nodes.btc_regtest_controller,
        60,
        || {
            let vrf_count = vrfs_submitted.load(Ordering::SeqCst);
            Ok(vrf_count >= 1)
        },
    )
    .unwrap();

    // second block should confirm the VRF register, wait until a block commit is submitted
    next_block_and(
        &mut signer_test.running_nodes.btc_regtest_controller,
        60,
        || {
            let commits_count = commits_submitted.load(Ordering::SeqCst);
            Ok(commits_count >= 1)
        },
    )
    .unwrap();

    // Mine 1 nakamoto tenure
    next_block_and_mine_commit(
        &mut signer_test.running_nodes.btc_regtest_controller,
        60,
        &signer_test.running_nodes.coord_channel,
        &commits_submitted,
    )
    .unwrap();

    let mut aggregate_public_key = None;
    let recv = signer_test
        .result_receivers
        .last()
        .expect("Failed to get recv");
    let results = recv
        .recv_timeout(Duration::from_secs(30))
        .expect("failed to recv dkg results");
    for result in results {
        match result {
            OperationResult::Dkg(point) => {
                info!("Received aggregate_group_key {point}");
                aggregate_public_key = Some(point);
                break;
            }
            _ => {
                panic!("Received Unexpected result");
            }
        }
    }
    let aggregate_public_key = aggregate_public_key.expect("Failed to get aggregate public key");

    info!("------------------------- Test Block Processed -------------------------");
    let recv = signer_test
        .result_receivers
        .last()
        .expect("Failed to retreive coordinator recv");
    let results = recv
        .recv_timeout(Duration::from_secs(30))
        .expect("failed to recv signature results");
    let mut signature = None;
    for result in results {
        match result {
            OperationResult::Sign(sig) => {
                info!("Received Signature ({},{})", &sig.R, &sig.z);
                signature = Some(sig);
                break;
            }
            _ => {
                panic!("Unexpected operation result");
            }
        }
    }
    let signature = signature.expect("Failed to get signature");
    // Wait for the block to show up in the test observer (Don't have to wait long as if we have received a signature,
    // we know that the signers have already received their block proposal events via their event observers)
    let t_start = Instant::now();
    while test_observer::get_proposal_responses().is_empty() {
        assert!(
            t_start.elapsed() < Duration::from_secs(30),
            "Timed out while waiting for block proposal event"
        );
        thread::sleep(Duration::from_secs(1));
    }
    let validate_responses = test_observer::get_proposal_responses();
    let proposed_signer_signature_hash =
        match validate_responses.first().expect("No block proposal") {
            BlockValidateResponse::Ok(block_validated) => block_validated.signer_signature_hash,
            _ => panic!("Unexpected response"),
        };
    assert!(
        signature.verify(
            &aggregate_public_key,
            proposed_signer_signature_hash.0.as_slice()
        ),
        "Signature verification failed"
    );
    // Verify that the signers broadcasted a signed NakamotoBlock back to the .signers contract
    let t_start = Instant::now();
    let mut chunk = None;
    while chunk.is_none() {
        assert!(
            t_start.elapsed() < Duration::from_secs(30),
            "Timed out while waiting for signers block response stacker db event"
        );

        let nakamoto_blocks = test_observer::get_stackerdb_chunks();
        for event in nakamoto_blocks {
            if event.contract_id.name == format!("signers-1-{}", BLOCK_MSG_ID).as_str().into()
                || event.contract_id.name == format!("signers-0-{}", BLOCK_MSG_ID).as_str().into()
            {
                for slot in event.modified_slots {
                    chunk = Some(slot.data);
                    break;
                }
                if chunk.is_some() {
                    break;
                }
            }
            if chunk.is_some() {
                break;
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    let chunk = chunk.unwrap();
    let signer_message = read_next::<SignerMessage, _>(&mut &chunk[..]).unwrap();
    if let SignerMessage::BlockResponse(BlockResponse::Accepted((
        block_signer_signature_hash,
        block_signature,
    ))) = signer_message
    {
        assert_eq!(block_signer_signature_hash, proposed_signer_signature_hash);
        assert_eq!(block_signature, ThresholdSignature(signature));
    } else {
        panic!("Received unexpected message");
    }
    signer_test.shutdown();
}

#[test]
#[ignore]
/// Test that signers will reject a miners block proposal if it is missing expected transactions
///
/// Test Setup:
/// The test spins up five stacks signers, one miner Nakamoto node, and a corresponding bitcoind.
/// The stacks node is advanced to epoch 3.0. and signers perform a DKG round (this should be removed
/// once we have proper casting of the vote during epoch 2.5).
///
/// Test Execution:
/// The node attempts to mine a Nakamoto tenure, sending a block to the observing signers via the
/// .miners stacker db instance. The signers submit the block to the stacks node for verification.
/// Upon receiving a Block Validation response approving the block, the signers verify that it contains
/// all expected transactions. As it does not, the signers reject the block and do not sign it.
///
/// Test Assertion:
/// Signers broadcast rejections with the list of missing transactions back to the miners stackerdb instance
fn stackerdb_block_proposal_missing_transactions() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    info!("------------------------- Test Setup -------------------------");
    let mut signer_test = SignerTest::new(5, 5);

    let host = signer_test
        .running_nodes
        .conf
        .node
        .rpc_bind
        .to_socket_addrs()
        .unwrap()
        .next()
        .unwrap();
    let _stx_genesissigner_stacker_db_1 = signer_test
        .running_nodes
        .conf
        .node
        .stacker_dbs
        .iter()
        .find(|id| {
            id.name.to_string() == NakamotoSigners::make_signers_db_name(1, TRANSACTIONS_MSG_ID)
        })
        .unwrap()
        .clone();

    let signer_id = 0;

    let signer_addresses_1: Vec<_> = signer_test
        .stacks_client
        .get_stackerdb_signer_slots(&boot_code_id(SIGNERS_NAME, false), 1)
        .unwrap()
        .into_iter()
        .map(|(address, _)| address)
        .collect();

    let signer_address_1 = signer_addresses_1.get(signer_id).cloned().unwrap();

    let signer_private_key_1 = signer_test
        .signer_stacks_private_keys
        .iter()
        .find(|pk| {
            let addr = StacksAddress::p2pkh(false, &StacksPublicKey::from_private(pk));
            addr == signer_address_1
        })
        .cloned()
        .expect("Cannot find signer private key for signer id 1");

    let mut stackerdb_1 = StackerDB::new(host, signer_private_key_1, false, 1, 0);

    debug!("Signer address is {}", &signer_address_1);
    assert_eq!(
        signer_address_1,
        StacksAddress::p2pkh(false, &StacksPublicKey::from_private(&signer_private_key_1))
    );

    // Create a valid transaction signed by the signer private key coresponding to the slot into which it is being inserted (signer id 0)
    let mut valid_tx = StacksTransaction {
        version: TransactionVersion::Testnet,
        chain_id: 0x80000000,
        auth: TransactionAuth::from_p2pkh(&signer_private_key_1).unwrap(),
        anchor_mode: TransactionAnchorMode::Any,
        post_condition_mode: TransactionPostConditionMode::Allow,
        post_conditions: vec![],
        payload: TransactionPayload::SmartContract(
            TransactionSmartContract {
                name: "test-contract".into(),
                code_body: StacksString::from_str("(/ 1 0)").unwrap(),
            },
            None,
        ),
    };
    valid_tx.set_origin_nonce(2);

    // Create a transaction signed by a different private key
    // This transaction will be invalid as it is signed by a non signer private key
    let invalid_signer_private_key = StacksPrivateKey::new();
    debug!(
        "Invalid address is {}",
        &StacksAddress::p2pkh(
            false,
            &StacksPublicKey::from_private(&invalid_signer_private_key)
        )
    );
    let mut invalid_tx = StacksTransaction {
        version: TransactionVersion::Testnet,
        chain_id: 0,
        auth: TransactionAuth::from_p2pkh(&invalid_signer_private_key).unwrap(),
        anchor_mode: TransactionAnchorMode::Any,
        post_condition_mode: TransactionPostConditionMode::Allow,
        post_conditions: vec![],
        payload: TransactionPayload::SmartContract(
            TransactionSmartContract {
                name: "test-contract".into(),
                code_body: StacksString::from_str("(/ 1 0)").unwrap(),
            },
            None,
        ),
    };
    invalid_tx.set_origin_nonce(0);

    // First run DKG in order to sign the block that arrives from the miners following a nakamoto block production
    // TODO: remove this forcibly running DKG once we have casting of the vote automagically happening during epoch 2.5
    info!("signer_runloop: spawn send commands to do dkg");
    let dkg_command = RunLoopCommand {
        reward_cycle: 0,
        command: SignerCommand::Dkg,
    };
    for cmd_sender in signer_test.signer_cmd_senders.values() {
        cmd_sender
            .send(dkg_command.clone())
            .expect("failed to send Dkg command");
    }
    let recv = signer_test
        .result_receivers
        .last()
        .expect("Failed to get coordinator recv");
    let results = recv
        .recv_timeout(Duration::from_secs(30))
        .expect("failed to recv dkg results");
    for result in results {
        match result {
            OperationResult::Dkg(point) => {
                info!("Received aggregate_group_key {point}");
                break;
            }
            _ => {
                panic!("Received Unexpected result");
            }
        }
    }

    // Following stacker DKG, submit transactions to stackerdb for the signers to pick up during block verification
    stackerdb_1
        .send_message_with_retry(SignerMessage::Transactions(vec![
            valid_tx.clone(),
            invalid_tx.clone(),
        ]))
        .expect("Failed to write expected transactions to stackerdb_1");

    let (vrfs_submitted, commits_submitted) = (
        signer_test.running_nodes.vrfs_submitted.clone(),
        signer_test.running_nodes.commits_submitted.clone(),
    );

    info!("------------------------- Test Block Rejected -------------------------");

    info!("Mining a Nakamoto tenure...");

    // first block wakes up the run loop, wait until a key registration has been submitted.
    next_block_and(
        &mut signer_test.running_nodes.btc_regtest_controller,
        60,
        || {
            let vrf_count = vrfs_submitted.load(Ordering::SeqCst);
            Ok(vrf_count >= 1)
        },
    )
    .unwrap();

    // second block should confirm the VRF register, wait until a block commit is submitted
    next_block_and(
        &mut signer_test.running_nodes.btc_regtest_controller,
        60,
        || {
            let commits_count = commits_submitted.load(Ordering::SeqCst);
            Ok(commits_count >= 1)
        },
    )
    .unwrap();

    // Mine 1 nakamoto tenure
    next_block_and_mine_commit(
        &mut signer_test.running_nodes.btc_regtest_controller,
        60,
        &signer_test.running_nodes.coord_channel,
        &commits_submitted,
    )
    .unwrap();

    // Verify that the signers broadcasted a series of rejections with missing transactions back to the miner
    let t_start = Instant::now();
    let mut chunk = None;
    while chunk.is_none() {
        assert!(
            t_start.elapsed() < Duration::from_secs(30),
            "Timed out while waiting for signers block response stacker db event"
        );

        let nakamoto_blocks = test_observer::get_stackerdb_chunks();
        for event in nakamoto_blocks {
            // Only care about the miners block slot
            if event.contract_id.name == format!("signers-1-{}", BLOCK_MSG_ID).as_str().into()
                || event.contract_id.name == format!("signers-0-{}", BLOCK_MSG_ID).as_str().into()
            {
                for slot in event.modified_slots {
                    chunk = Some(slot.data);
                    break;
                }
                if chunk.is_some() {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    let chunk = chunk.unwrap();
    let signer_message = read_next::<SignerMessage, _>(&mut &chunk[..]).unwrap();
    if let SignerMessage::BlockResponse(BlockResponse::Rejected(block_rejection)) = signer_message {
        // Verify we are missing the valid tx that we expect to see in the block
        if let RejectCode::MissingTransactions(missing_txs) = block_rejection.reason_code {
            assert_eq!(missing_txs, vec![valid_tx]);
        } else {
            panic!("Received unexpected rejection reason");
        }
    } else {
        panic!("Received unexpected message: {:?}", &signer_message);
    }
    signer_test.shutdown();
}
