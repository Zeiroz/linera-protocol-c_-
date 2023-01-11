// Copyright (c) Facebook, Inc. and its affiliates.
// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! WASM specific worker tests.

use super::{init_worker_with_chains, make_block, make_certificate, make_state_hash};
use linera_base::{
    crypto::KeyPair,
    data_types::{BlockHeight, ChainDescription, ChainId, Epoch},
};
use linera_chain::data_types::{Event, Message, Origin, Value};
use linera_execution::{
    system::{Balance, SystemChannel, SystemEffect, SystemOperation},
    ApplicationId, Bytecode, ChainOwnership, ChannelId, Destination, Effect, ExecutionStateView,
    SystemExecutionState,
};
use linera_storage::{DynamoDbStoreClient, MemoryStoreClient, RocksdbStoreClient, Store};
use linera_views::{
    test_utils::LocalStackTestContext,
    views::{HashableContainerView, ViewError},
};
use std::collections::BTreeSet;
use test_log::test;

#[test(tokio::test)]
async fn test_memory_handle_certificates_to_create_application() -> Result<(), anyhow::Error> {
    let client = MemoryStoreClient::default();
    run_test_handle_certificates_to_create_application(client).await
}

#[test(tokio::test)]
async fn test_rocksdb_handle_certificates_to_create_application() -> Result<(), anyhow::Error> {
    let dir = tempfile::TempDir::new().unwrap();
    let client = RocksdbStoreClient::new(dir.path().to_path_buf());
    run_test_handle_certificates_to_create_application(client).await
}

#[test(tokio::test)]
#[ignore]
async fn test_dynamo_db_handle_certificates_to_create_application() -> Result<(), anyhow::Error> {
    let table = "linera".parse().expect("Invalid table name");
    let localstack = LocalStackTestContext::new().await?;
    let (client, _) =
        DynamoDbStoreClient::from_config(localstack.dynamo_db_config(), table).await?;
    run_test_handle_certificates_to_create_application(client).await
}

async fn run_test_handle_certificates_to_create_application<S>(
    client: S,
) -> Result<(), anyhow::Error>
where
    S: Store + Clone + Send + Sync + 'static,
    ViewError: From<S::ContextError>,
{
    let admin_id = ChainDescription::Root(0);
    let publisher_key_pair = KeyPair::generate();
    let publisher_chain = ChainDescription::Root(1);
    let creator_key_pair = KeyPair::generate();
    let creator_chain = ChainDescription::Root(2);
    let (committee, mut worker) = init_worker_with_chains(
        client,
        vec![
            (
                publisher_chain,
                publisher_key_pair.public(),
                Balance::from(0),
            ),
            (creator_chain, creator_key_pair.public(), Balance::from(0)),
        ],
    )
    .await;

    // Publish some bytecode.
    let publish_operation = SystemOperation::PublishBytecode {
        contract: Bytecode::load_from_file(
            "../target/wasm32-unknown-unknown/debug/examples/counter_contract.wasm",
        )
        .await?,
        service: Bytecode::load_from_file(
            "../target/wasm32-unknown-unknown/debug/examples/counter_service.wasm",
        )
        .await?,
    };
    let publish_effect = SystemEffect::BytecodePublished.into();
    let publish_block = make_block(
        Epoch::from(0),
        publisher_chain.into(),
        vec![publish_operation],
        vec![],
        None,
    );
    let publish_block_height = publish_block.height;
    let publish_channel = Destination::Subscribers(SystemChannel::PublishedBytecodes.name());
    let publisher_system_state = SystemExecutionState {
        epoch: Some(Epoch::from(0)),
        description: Some(publisher_chain),
        admin_id: Some(admin_id.into()),
        subscriptions: BTreeSet::new(),
        committees: [(Epoch::from(0), committee.clone())].into_iter().collect(),
        ownership: ChainOwnership::single(publisher_key_pair.public().into()),
        balance: Balance::from(0),
    };
    let publisher_state_hash = make_state_hash(publisher_system_state).await;
    let publish_block_proposal = Value::ConfirmedBlock {
        block: publish_block,
        effects: vec![(
            ApplicationId::System,
            publish_channel,
            Effect::System(publish_effect),
        )],
        state_hash: publisher_state_hash,
    };
    let publish_certificate = make_certificate(&committee, &worker, publish_block_proposal);

    let info = worker
        .fully_handle_certificate(publish_certificate.clone())
        .await
        .unwrap()
        .info;
    assert_eq!(ChainId::from(publisher_chain), info.chain_id);
    assert_eq!(Balance::from(0), info.system_balance);
    assert_eq!(BlockHeight::from(1), info.next_block_height);
    assert_eq!(Some(publish_certificate.hash), info.block_hash);
    assert!(info.manager.pending().is_none());

    // Subscribe to get the bytecode ID.
    let subscribe_operation = SystemOperation::Subscribe {
        chain_id: publisher_chain.into(),
        channel: SystemChannel::PublishedBytecodes,
    };
    let publisher_channel = ChannelId {
        chain_id: publisher_chain.into(),
        name: SystemChannel::PublishedBytecodes.name(),
    };
    let subscribe_effect = SystemEffect::Subscribe {
        id: creator_chain.into(),
        channel: publisher_channel.clone(),
    };
    let subscribe_block = make_block(
        Epoch::from(0),
        creator_chain.into(),
        vec![subscribe_operation],
        vec![],
        None,
    );
    let subscribe_block_height = subscribe_block.height;
    let creator_system_state = SystemExecutionState {
        epoch: Some(Epoch::from(0)),
        description: Some(creator_chain),
        admin_id: Some(admin_id.into()),
        subscriptions: [publisher_channel].into_iter().collect(),
        committees: [(Epoch::from(0), committee.clone())].into_iter().collect(),
        ownership: ChainOwnership::single(creator_key_pair.public().into()),
        balance: Balance::from(0),
    };
    let mut creator_state = ExecutionStateView::from_system_state(creator_system_state).await;
    let subscribe_block_proposal = Value::ConfirmedBlock {
        block: subscribe_block,
        effects: vec![(
            ApplicationId::System,
            Destination::Recipient(publisher_chain.into()),
            Effect::System(subscribe_effect.clone()),
        )],
        state_hash: creator_state.hash_value().await?,
    };
    let subscribe_certificate = make_certificate(&committee, &worker, subscribe_block_proposal);

    let info = worker
        .fully_handle_certificate(subscribe_certificate.clone())
        .await
        .unwrap()
        .info;
    assert_eq!(ChainId::from(creator_chain), info.chain_id);
    assert_eq!(Balance::from(0), info.system_balance);
    assert_eq!(BlockHeight::from(1), info.next_block_height);
    assert_eq!(Some(subscribe_certificate.hash), info.block_hash);
    assert!(info.manager.pending().is_none());

    // Accept subscription
    let accept_message = Message {
        application_id: ApplicationId::System,
        origin: Origin::chain(creator_chain.into()),
        event: Event {
            certificate_hash: subscribe_certificate.hash,
            height: subscribe_block_height,
            index: 0,
            effect: subscribe_effect.into(),
        },
    };
    let accept_block = make_block(
        Epoch::from(0),
        publisher_chain.into(),
        Vec::<SystemOperation>::new(),
        vec![accept_message],
        Some(&publish_certificate),
    );
    let accept_block_proposal = Value::ConfirmedBlock {
        block: accept_block,
        effects: vec![(
            ApplicationId::System,
            Destination::Recipient(creator_chain.into()),
            Effect::System(SystemEffect::Notify {
                id: creator_chain.into(),
            }),
        )],
        state_hash: publisher_state_hash,
    };
    let accept_certificate = make_certificate(&committee, &worker, accept_block_proposal);

    let info = worker
        .fully_handle_certificate(accept_certificate.clone())
        .await
        .unwrap()
        .info;
    assert_eq!(ChainId::from(publisher_chain), info.chain_id);
    assert_eq!(Balance::from(0), info.system_balance);
    assert_eq!(BlockHeight::from(2), info.next_block_height);
    assert_eq!(Some(accept_certificate.hash), info.block_hash);
    assert!(info.manager.pending().is_none());

    todo!();
}
