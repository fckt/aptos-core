// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    local_client::LocalClient,
    persistent_safety_storage::PersistentSafetyStorage,
    process::ProcessService,
    remote_service::RemoteService,
    serializer::{SerializerClient, SerializerService},
    thread::ThreadService,
    SafetyRules, TSafetyRules,
};
use aptos_config::config::{InitialSafetyRulesConfig, SafetyRulesConfig, SafetyRulesService};
use aptos_infallible::RwLock;
use aptos_secure_storage::{KVStorage, Storage};
use std::{convert::TryInto, net::SocketAddr, sync::Arc};

pub fn storage(config: &SafetyRulesConfig) -> PersistentSafetyStorage {
    let backend = &config.backend;
    let internal_storage: Storage = backend.try_into().expect("Unable to initialize storage");
    if let Err(error) = internal_storage.available() {
        panic!("Storage is not available: {:?}", error);
    }

    if let Some(test_config) = &config.test {
        let author = test_config.author;
        let consensus_private_key = test_config
            .consensus_key
            .as_ref()
            .expect("Missing consensus key in test config")
            .private_key();
        let execution_private_key = test_config
            .execution_key
            .as_ref()
            .expect("Missing execution key in test config")
            .private_key();
        let waypoint = test_config.waypoint.expect("No waypoint in config");

        PersistentSafetyStorage::initialize(
            internal_storage,
            author,
            consensus_private_key,
            execution_private_key,
            waypoint,
            config.enable_cached_safety_data,
        )
    } else {
        let storage =
            PersistentSafetyStorage::new(internal_storage, config.enable_cached_safety_data);
        // If it's initialized, then we can continue
        if storage.author().is_ok() {
            storage
        } else if !matches!(
            config.initial_safety_rules_config,
            InitialSafetyRulesConfig::None
        ) {
            let identity_blob = config.initial_safety_rules_config.identity_blob();
            let waypoint = config.initial_safety_rules_config.waypoint();

            let backend = &config.backend;
            let internal_storage: Storage =
                backend.try_into().expect("Unable to initialize storage");
            PersistentSafetyStorage::initialize(
                internal_storage,
                identity_blob
                    .account_address
                    .expect("AccountAddress needed for safety rules"),
                identity_blob
                    .consensus_private_key
                    .expect("Consensus key needed for safety rules"),
                identity_blob
                    .account_private_key
                    .expect("Account key needed for safety rules"),
                waypoint,
                config.enable_cached_safety_data,
            )
        } else {
            panic!(
                "Safety rules storage is not initialized, provide an initial safety rules config"
            )
        }
    }
}

enum SafetyRulesWrapper {
    Local(Arc<RwLock<SafetyRules>>),
    Process(ProcessService),
    Serializer(Arc<RwLock<SerializerService>>),
    Thread(ThreadService),
}

pub struct SafetyRulesManager {
    internal_safety_rules: SafetyRulesWrapper,
}

impl SafetyRulesManager {
    pub fn new(config: &SafetyRulesConfig) -> Self {
        if let SafetyRulesService::Process(conf) = &config.service {
            return Self::new_process(conf.server_address(), config.network_timeout_ms);
        }

        let storage = storage(config);
        let verify_vote_proposal_signature = config.verify_vote_proposal_signature;
        let export_consensus_key = config.export_consensus_key;
        match config.service {
            SafetyRulesService::Local => Self::new_local(
                storage,
                verify_vote_proposal_signature,
                export_consensus_key,
            ),
            SafetyRulesService::Serializer => Self::new_serializer(
                storage,
                verify_vote_proposal_signature,
                export_consensus_key,
            ),
            SafetyRulesService::Thread => Self::new_thread(
                storage,
                verify_vote_proposal_signature,
                export_consensus_key,
                config.network_timeout_ms,
            ),
            _ => panic!("Unimplemented SafetyRulesService: {:?}", config.service),
        }
    }

    pub fn new_local(
        storage: PersistentSafetyStorage,
        verify_vote_proposal_signature: bool,
        export_consensus_key: bool,
    ) -> Self {
        let safety_rules = SafetyRules::new(
            storage,
            verify_vote_proposal_signature,
            export_consensus_key,
        );
        Self {
            internal_safety_rules: SafetyRulesWrapper::Local(Arc::new(RwLock::new(safety_rules))),
        }
    }

    pub fn new_process(server_addr: SocketAddr, timeout_ms: u64) -> Self {
        let process_service = ProcessService::new(server_addr, timeout_ms);
        Self {
            internal_safety_rules: SafetyRulesWrapper::Process(process_service),
        }
    }

    pub fn new_serializer(
        storage: PersistentSafetyStorage,
        verify_vote_proposal_signature: bool,
        export_consensus_key: bool,
    ) -> Self {
        let safety_rules = SafetyRules::new(
            storage,
            verify_vote_proposal_signature,
            export_consensus_key,
        );
        let serializer_service = SerializerService::new(safety_rules);
        Self {
            internal_safety_rules: SafetyRulesWrapper::Serializer(Arc::new(RwLock::new(
                serializer_service,
            ))),
        }
    }

    pub fn new_thread(
        storage: PersistentSafetyStorage,
        verify_vote_proposal_signature: bool,
        export_consensus_key: bool,
        timeout_ms: u64,
    ) -> Self {
        let thread = ThreadService::new(
            storage,
            verify_vote_proposal_signature,
            export_consensus_key,
            timeout_ms,
        );
        Self {
            internal_safety_rules: SafetyRulesWrapper::Thread(thread),
        }
    }

    pub fn client(&self) -> Box<dyn TSafetyRules + Send + Sync> {
        match &self.internal_safety_rules {
            SafetyRulesWrapper::Local(safety_rules) => {
                Box::new(LocalClient::new(safety_rules.clone()))
            }
            SafetyRulesWrapper::Process(process) => Box::new(process.client()),
            SafetyRulesWrapper::Serializer(serializer_service) => {
                Box::new(SerializerClient::new(serializer_service.clone()))
            }
            SafetyRulesWrapper::Thread(thread) => Box::new(thread.client()),
        }
    }
}
