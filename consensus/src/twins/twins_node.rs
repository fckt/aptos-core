// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    commit_notifier::QuorumStoreCommitNotifier,
    counters,
    epoch_manager::EpochManager,
    network::NetworkTask,
    network_interface::{ConsensusNetworkEvents, ConsensusNetworkSender},
    network_tests::{NetworkPlayground, TwinId},
    test_utils::{MockStateComputer, MockStorage},
    util::time_service::ClockTimeService,
};
use aptos_config::{
    config::{
        ConsensusProposerType::{self, RoundProposer},
        NodeConfig, WaypointConfig,
    },
    generator::{self, ValidatorSwarm},
    network_id::NetworkId,
};
use aptos_mempool::mocks::MockSharedMempool;
use aptos_types::{
    ledger_info::LedgerInfoWithSignatures,
    on_chain_config::{OnChainConfig, OnChainConfigPayload, ValidatorSet},
    transaction::SignedTransaction,
    validator_info::ValidatorInfo,
    waypoint::Waypoint,
};
use channel::{self, aptos_channel, message_queues::QueueStyle};
use consensus_types::common::{Author, Round};
use event_notifications::{ReconfigNotification, ReconfigNotificationListener};
use futures::channel::mpsc;
use network::{
    peer_manager::{conn_notifs_channel, ConnectionRequestSender, PeerManagerRequestSender},
    protocols::{
        network::{NewNetworkEvents, NewNetworkSender},
        wire::handshake::v1::ProtocolIdSet,
    },
    transport::ConnectionMetadata,
    ProtocolId,
};
use std::{collections::HashMap, iter::FromIterator, sync::Arc};
use tokio::runtime::{Builder, Runtime};

/// Auxiliary struct that is preparing SMR for the test
pub struct SMRNode {
    pub id: TwinId,
    pub storage: Arc<MockStorage>,
    pub commit_cb_receiver: mpsc::UnboundedReceiver<LedgerInfoWithSignatures>,
    _runtime: Runtime,
    _shared_mempool: MockSharedMempool,
    _state_sync: mpsc::UnboundedReceiver<Vec<SignedTransaction>>,
}

fn author_from_config(config: &NodeConfig) -> Author {
    config.validator_network.as_ref().unwrap().peer_id()
}

impl SMRNode {
    fn start(
        playground: &mut NetworkPlayground,
        config: NodeConfig,
        storage: Arc<MockStorage>,
        twin_id: TwinId,
    ) -> Self {
        let (network_reqs_tx, network_reqs_rx) = aptos_channel::new(QueueStyle::FIFO, 8, None);
        let (connection_reqs_tx, _) = aptos_channel::new(QueueStyle::FIFO, 8, None);
        let (consensus_tx, consensus_rx) = aptos_channel::new(QueueStyle::FIFO, 8, None);
        let (_conn_mgr_reqs_tx, conn_mgr_reqs_rx) = channel::new_test(8);
        let (_, conn_notifs_channel) = conn_notifs_channel::new();
        let mut network_sender = ConsensusNetworkSender::new(
            PeerManagerRequestSender::new(network_reqs_tx),
            ConnectionRequestSender::new(connection_reqs_tx),
        );
        network_sender.initialize(playground.peer_protocols());
        let network_events = ConsensusNetworkEvents::new(consensus_rx, conn_notifs_channel);

        playground.add_node(twin_id, consensus_tx, network_reqs_rx, conn_mgr_reqs_rx);

        let (state_sync_client, state_sync) = mpsc::unbounded();
        let (commit_cb_sender, commit_cb_receiver) = mpsc::unbounded::<LedgerInfoWithSignatures>();
        let shared_mempool = MockSharedMempool::new();
        let (quorum_store_to_mempool_sender, _) = mpsc::channel(1_024);
        let state_computer = Arc::new(MockStateComputer::new(
            state_sync_client,
            commit_cb_sender,
            Arc::clone(&storage),
        ));
        let (reconfig_sender, reconfig_events) = aptos_channel::new(QueueStyle::LIFO, 1, None);
        let reconfig_listener = ReconfigNotificationListener {
            notification_receiver: reconfig_events,
        };
        let commit_notifier = Arc::new(QuorumStoreCommitNotifier::new(1_000));
        let mut configs = HashMap::new();
        configs.insert(
            ValidatorSet::CONFIG_ID,
            bcs::to_bytes(storage.get_validator_set()).unwrap(),
        );
        let payload = OnChainConfigPayload::new(1, Arc::new(configs));
        reconfig_sender
            .push(
                (),
                ReconfigNotification {
                    version: 1,
                    on_chain_configs: payload,
                },
            )
            .unwrap();

        let runtime = Builder::new_multi_thread()
            .thread_name(format!(
                "{}-node-{}",
                twin_id.id,
                std::thread::current().name().unwrap_or("")
            ))
            .enable_all()
            .build()
            .unwrap();

        let time_service = Arc::new(ClockTimeService::new(runtime.handle().clone()));

        let (timeout_sender, timeout_receiver) =
            channel::new(1_024, &counters::PENDING_ROUND_TIMEOUTS);
        let (self_sender, self_receiver) = channel::new(1_024, &counters::PENDING_SELF_MESSAGES);

        let epoch_mgr = EpochManager::new(
            &config,
            time_service,
            self_sender,
            network_sender,
            timeout_sender,
            quorum_store_to_mempool_sender,
            state_computer,
            storage.clone(),
            reconfig_listener,
            commit_notifier,
        );
        let (network_task, network_receiver) = NetworkTask::new(network_events, self_receiver);

        runtime.spawn(network_task.start());
        runtime.spawn(epoch_mgr.start(timeout_receiver, network_receiver));
        Self {
            id: twin_id,
            _runtime: runtime,
            commit_cb_receiver,
            storage,
            _shared_mempool: shared_mempool,
            _state_sync: state_sync,
        }
    }

    /// Starts a given number of nodes and their twins
    pub fn start_num_nodes_with_twins(
        num_nodes: usize,
        num_twins: usize,
        playground: &mut NetworkPlayground,
        proposer_type: ConsensusProposerType,
        round_proposers_idx: Option<HashMap<Round, usize>>,
    ) -> Vec<Self> {
        assert!(num_nodes >= num_twins);
        let ValidatorSwarm {
            nodes: mut node_configs,
        } = generator::validator_swarm_for_testing(num_nodes);
        let peer_metadata_storage = playground.peer_protocols();
        node_configs.iter().for_each(|config| {
            let mut conn_meta = ConnectionMetadata::mock(author_from_config(config));
            conn_meta.application_protocols = ProtocolIdSet::from_iter([
                ProtocolId::ConsensusDirectSendJson,
                ProtocolId::ConsensusDirectSendBcs,
                ProtocolId::ConsensusRpcBcs,
            ]);
            peer_metadata_storage.insert_connection(NetworkId::Validator, conn_meta);
        });

        let validator_set = ValidatorSet::new(
            node_configs
                .iter()
                .map(|config| {
                    let sr_test_config = config.consensus.safety_rules.test.as_ref().unwrap();
                    ValidatorInfo::new_with_test_network_keys(
                        sr_test_config.author,
                        sr_test_config.consensus_key.as_ref().unwrap().public_key(),
                        1,
                    )
                })
                .collect(),
        );
        // sort by the peer id
        node_configs.sort_by_key(author_from_config);

        let proposer_type = match proposer_type {
            RoundProposer(_) => {
                let mut round_proposers: HashMap<Round, Author> = HashMap::new();

                if let Some(proposers) = round_proposers_idx {
                    proposers.iter().for_each(|(round, idx)| {
                        round_proposers.insert(*round, author_from_config(&node_configs[*idx]));
                    })
                }
                RoundProposer(round_proposers)
            }
            _ => proposer_type,
        };

        // We don't add twins to ValidatorSet or round_proposers above
        // because a node with twins should be treated the same at the
        // consensus level
        for i in 0..num_twins {
            let twin = node_configs[i].clone();
            node_configs.push(twin);
        }

        let mut smr_nodes = vec![];

        for (smr_id, mut config) in node_configs.into_iter().enumerate() {
            let (_, storage) = MockStorage::start_for_testing(validator_set.clone());

            let waypoint = Waypoint::new_epoch_boundary(&storage.get_ledger_info())
                .expect("Unable to produce waypoint with the provided LedgerInfo");
            config
                .consensus
                .safety_rules
                .test
                .as_mut()
                .unwrap()
                .waypoint = Some(waypoint);
            config.base.waypoint = WaypointConfig::FromConfig(waypoint);
            config.consensus.proposer_type = proposer_type.clone();
            config.consensus.safety_rules.verify_vote_proposal_signature = false;
            // Disable timeout in twins test to avoid flakiness
            config.consensus.round_initial_timeout_ms = 2_000_000;

            let author = author_from_config(&config);

            let twin_id = TwinId { id: smr_id, author };

            smr_nodes.push(Self::start(playground, config, storage, twin_id));
        }
        smr_nodes
    }
}
