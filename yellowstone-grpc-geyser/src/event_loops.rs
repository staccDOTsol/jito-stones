use std::{collections::BTreeMap, sync::Arc, time::Duration};

use tokio::sync::mpsc::UnboundedSender;
use futures_util::{SinkExt, StreamExt};
use jito_protos::{
    bundle::BundleResult, convert::versioned_tx_from_packet, searcher::{
        mempool_subscription, MempoolSubscription, PendingTxNotification,
        SubscribeBundleResultsRequest, WriteLockedAccountSubscriptionV0,
    }
};
use tokio::        time::{sleep, Instant};

use log::error;
use solana_sdk::transaction::AddressLoader;

use jito_searcher_client::get_searcher_client;
use log::info;
use solana_client::{
    nonblocking::pubsub_client::PubsubClient, rpc_client::RpcClient, rpc_config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter}, rpc_response, rpc_response::{RpcBlockUpdate, SlotUpdate}
};
use solana_metrics::{datapoint_error, datapoint_info};
use solana_sdk::{
    clock::Slot, commitment_config::{CommitmentConfig, CommitmentLevel}, message::v0::LoadedAddresses, pubkey::Pubkey, signature::Keypair, transaction::{SimpleAddressLoader, SanitizedTransaction, SanitizedVersionedTransaction}
};
use yellowstone_grpc_proto::geyser::CommitmentLevel as cl2;
use solana_transaction_status::{TransactionDetails, TransactionStatusMeta, UiTransactionEncoding};
use tokio::{sync::mpsc::Sender};
use tonic::Streaming;

use crate::grpc::{self, MessageTransactionInfo, SlotMessages};

// slot update subscription loop that attempts to maintain a connection to an RPC server
pub async fn slot_subscribe_loop(pubsub_addr: String, slot_sender: Sender<Slot>) {
    let mut connect_errors: u64 = 0;
    let mut slot_subscribe_errors: u64 = 0;
    let mut slot_subscribe_disconnect_errors: u64 = 0;

    loop {
        sleep(Duration::from_secs(1)).await;

        match PubsubClient::new(&pubsub_addr).await {
            Ok(pubsub_client) => match pubsub_client.slot_updates_subscribe().await {
                Ok((mut slot_update_subscription, _unsubscribe_fn)) => {
                    while let Some(slot_update) = slot_update_subscription.next().await {
                        if let SlotUpdate::FirstShredReceived { slot, timestamp: _ } = slot_update {
                            datapoint_info!("slot_subscribe_slot", ("slot", slot, i64));
                            if slot_sender.send(slot).await.is_err() {
                                datapoint_error!("slot_subscribe_send_error", ("errors", 1, i64));
                                return;
                            }
                        }
                    }
                    slot_subscribe_disconnect_errors += 1;
                    datapoint_error!(
                        "slot_subscribe_disconnect_error",
                        ("errors", slot_subscribe_disconnect_errors, i64)
                    );
                }
                Err(e) => {
                    slot_subscribe_errors += 1;
                    datapoint_error!(
                        "slot_subscribe_error",
                        ("errors", slot_subscribe_errors, i64),
                        ("error_str", e.to_string(), String),
                    );
                }
            },
            Err(e) => {
                connect_errors += 1;
                datapoint_error!(
                    "slot_subscribe_pubsub_connect_error",
                    ("errors", connect_errors, i64),
                    ("error_str", e.to_string(), String)
                );
            }
        }
    }
}

// block subscription loop that attempts to maintain a connection to an RPC server
// NOTE: you must have --rpc-pubsub-enable-block-subscription and relevant flags started
// on your RPC servers for this to work.
pub async fn block_subscribe_loop(
    pubsub_addr: String,
    block_receiver: Sender<rpc_response::Response<RpcBlockUpdate>>,
) {
    let mut connect_errors: u64 = 0;
    let mut block_subscribe_errors: u64 = 0;
    let mut block_subscribe_disconnect_errors: u64 = 0;

    loop {
        sleep(Duration::from_secs(1)).await;

        match PubsubClient::new(&pubsub_addr).await {
            Ok(pubsub_client) => match pubsub_client
                .block_subscribe(
                    RpcBlockSubscribeFilter::All,
                    Some(RpcBlockSubscribeConfig {
                        commitment: Some(CommitmentConfig {
                            commitment: CommitmentLevel::Confirmed,
                        }),
                        encoding: Some(UiTransactionEncoding::Base64),
                        transaction_details: Some(TransactionDetails::Signatures),
                        show_rewards: Some(true),
                        max_supported_transaction_version: None,
                    }),
                )
                .await
            {
                Ok((mut block_update_subscription, _unsubscribe_fn)) => {
                    while let Some(block_update) = block_update_subscription.next().await {
                        datapoint_info!(
                            "block_subscribe_slot",
                            ("slot", block_update.context.slot, i64)
                        );
                        if block_receiver.send(block_update).await.is_err() {
                            datapoint_error!("block_subscribe_send_error", ("errors", 1, i64));
                            return;
                        }
                    }
                    block_subscribe_disconnect_errors += 1;
                    datapoint_error!(
                        "block_subscribe_disconnect_error",
                        ("errors", block_subscribe_disconnect_errors, i64)
                    );
                }
                Err(e) => {
                    block_subscribe_errors += 1;
                    datapoint_error!(
                        "block_subscribe_error",
                        ("errors", block_subscribe_errors, i64),
                        ("error_str", e.to_string(), String),
                    );
                }
            },
            Err(e) => {
                connect_errors += 1;
                datapoint_error!(
                    "block_subscribe_pubsub_connect_error",
                    ("errors", connect_errors, i64),
                    ("error_str", e.to_string(), String)
                );
            }
        }
    }
}

// attempts to maintain connection to searcher service and stream pending transaction notifications over a channel
pub async fn pending_tx_loop(
    block_engine_addr: String,
    auth_keypair: Arc<Keypair>,
    backrun_pubkeys: Vec<Pubkey>,
    broadcast_tx: tokio::sync::broadcast::Sender<(yellowstone_grpc_proto::geyser::CommitmentLevel, Arc<Vec<grpc::Message>>)>
) {
    let mut num_searcher_connection_errors: usize = 0;
    let mut num_pending_tx_sub_errors: usize = 0;
    let mut num_pending_tx_stream_errors: usize = 0;
    let mut num_pending_tx_stream_disconnects: usize = 0;

    info!("backrun pubkeys: {:?}", backrun_pubkeys);

    const PROCESSED_MESSAGES_MAX: usize = 31;
    const PROCESSED_MESSAGES_SLEEP: Duration = Duration::from_millis(10);
    
    let mut messages: BTreeMap<u64, SlotMessages> = Default::default();
    let mut processed_messages = Vec::with_capacity(PROCESSED_MESSAGES_MAX);
    let mut processed_sleep = sleep(PROCESSED_MESSAGES_SLEEP);
    let rpc = RpcClient::new("https://jarrett-solana-7ba9.mainnet.rpcpool.com/8d890735-edf2-4a75-af84-92f7c9e31718".to_string());
    tokio::pin!(processed_sleep);

    loop {
        sleep(Duration::from_secs(1)).await;

        match get_searcher_client(&block_engine_addr, &auth_keypair).await {
            Ok(mut searcher_client) => {
                match searcher_client
                    .subscribe_mempool(MempoolSubscription {
                        regions: vec![],
                        msg: Some(mempool_subscription::Msg::WlaV0Sub(
                            WriteLockedAccountSubscriptionV0 {
                                accounts: backrun_pubkeys.iter().map(|pk| pk.to_string()).collect(),
                            },
                        )),
                    })
                    .await
                {
                    Ok(pending_tx_stream_response) => {
                        let mut pending_tx_stream = pending_tx_stream_response.into_inner();
                        while let Some(maybe_notification) = pending_tx_stream.next().await {
                            match maybe_notification {
                                Ok(notification) => {
                                        let mut index = 0;
                                        // add these to the messages_rx as Tranasctions
                                        for packet in notification
                                        .transactions {
                                
                                            let transaction = versioned_tx_from_packet(&packet);
                                            if transaction.is_none(){
                                                info!("tx is none");
                                                continue;

                                            }
                                            let transaction = transaction.unwrap();
                            
                                            let signature = transaction.clone().signatures[0];
                                            let sanitized_tx = SanitizedVersionedTransaction::try_from(transaction.clone());
                                            if sanitized_tx.is_err() {
                                                error!("sanitized_tx error: {:?}", sanitized_tx.err().unwrap().to_string());
                                                continue;
                                            }
                                            let sanitized_tx = sanitized_tx.unwrap();
                                            let lookups = transaction.message.address_table_lookups();
                                            let mut address_loader = SimpleAddressLoader::Disabled;
                                            if lookups.is_some(){
                                                let lookups = lookups.unwrap();
                                             let address_loader_maybe =
                                                SimpleAddressLoader::load_addresses(
                                                    SimpleAddressLoader::Enabled(LoadedAddresses::default()),
                                                    &lookups
                                                );
                                                if address_loader_maybe.is_ok(){
                                                    address_loader = solana_sdk::transaction::SimpleAddressLoader::Enabled(address_loader_maybe.unwrap());
                                                }
                                                
                                        }
                                            let sanitized_tx = SanitizedTransaction::try_new(
                                                sanitized_tx,
                                                transaction.message.hash(),
                                                false,
                                                address_loader
                                            );
                                            if !sanitized_tx.is_err() {
                                                
                                            let sanitized_tx = sanitized_tx.unwrap();
                                            info!("got tx: {:?}", sanitized_tx);
                                            let message: grpc::Message = grpc::Message::Transaction(
                                                crate::grpc::MessageTransaction {
                                                    transaction: MessageTransactionInfo {
                                                        transaction: sanitized_tx,
                                                        signature,
                                                        is_vote: false,
                                                        meta: TransactionStatusMeta::default(),
                                                        index
                                                    },
                                                    slot: rpc.get_slot().unwrap(),
                                                },
                                            );
                                            
                                
                                                    let mut sealed_block_msg = None;

                                                    let slot_messages = messages.entry(message.get_slot()).or_default();
                                                    // Update block reconstruction info
                                                    
                                                    // Remove outdated block reconstruction info
                                                    match &message {
                                    
                                                        grpc::Message::Transaction(msg) => {
                                
                                                            slot_messages.transactions.push(msg.transaction.clone());
                                                            sealed_block_msg = slot_messages.try_seal();
                                                            
                                                        }
                                                        _ => {}
                                                    }
                                                    let mut messages_vec = vec![message];
                                
                                                    if let Some(sealed_block_msg) = sealed_block_msg {
                                                        messages_vec.push(sealed_block_msg);
                                                    }
                                
                                                    for message in messages_vec {
                                                      
                                                        if let Some(slot_messages) = messages.get_mut(&message.get_slot()) {    
                                                            let mut confirmed_messages = vec![];
                                                            let mut finalized_messages = vec![];
                                                            if matches!(message, grpc::Message::Block(_)) {
                                                                if let Some(slot_messages) = messages.get(&message.get_slot()) {
                                                                    if let Some(confirmed_at) = slot_messages.confirmed_at {
                                                                        confirmed_messages.extend(
                                                                            slot_messages.messages.as_slice()[confirmed_at..].iter().filter_map(|x| x.clone())
                                                                        );
                                                                    }
                                                                    if let Some(finalized_at) = slot_messages.finalized_at {
                                                                        finalized_messages.extend(
                                                                            slot_messages.messages.as_slice()[finalized_at..].iter().filter_map(|x| x.clone())
                                                                        );
                                                                    }
                                                                }
                                                            }
                                
                                                            processed_messages.push(message);
                                                            if processed_messages.len() >= PROCESSED_MESSAGES_MAX
                                                                || !confirmed_messages.is_empty()
                                                                || !finalized_messages.is_empty()
                                                            {
                                                                let _ = broadcast_tx
                                                                    .send((cl2::Processed, processed_messages.into()));
                                                                processed_messages = Vec::with_capacity(PROCESSED_MESSAGES_MAX);
                                                                processed_sleep
                                                                    .as_mut()
                                                                    .reset(Instant::now() + PROCESSED_MESSAGES_SLEEP);
                                                            }
                                
                                                            if !confirmed_messages.is_empty() {
                                                                let _ =
                                                                    broadcast_tx.send((cl2::Confirmed, confirmed_messages.into()));
                                                            }
                                
                                                            if !finalized_messages.is_empty() {
                                                                let _ =
                                                                    broadcast_tx.send((cl2::Finalized, finalized_messages.into()));
                                                            }
                                                        }
                                                    }
                                                }
                                                else {
                                                    error!("sanitized_tx error: {:?}", sanitized_tx.err().unwrap().to_string());
                                                }
                                                
                                                if !processed_messages.is_empty() {
                                                    let _ = broadcast_tx.send((cl2::Processed, processed_messages.into()));
                                                    processed_messages = Vec::with_capacity(PROCESSED_MESSAGES_MAX);
                                                    processed_sleep.as_mut().reset(Instant::now() + PROCESSED_MESSAGES_SLEEP);

                                                }
                                            }

                                              
                                        index += 1;
                                        }
                                        
                            
                                Err(e) => {
                                    num_pending_tx_stream_errors += 1;
                                    datapoint_error!(
                                        "searcher_pending_tx_stream_error",
                                        ("errors", num_pending_tx_stream_errors, i64),
                                        ("error_str", e.to_string(), String)
                                    );
                                    break;
                                }
                            }
                    }
                        num_pending_tx_stream_disconnects += 1;
                        datapoint_error!(
                            "searcher_pending_tx_stream_disconnect",
                            ("errors", num_pending_tx_stream_disconnects, i64),
                        );
                    }
                    Err(e) => {
                        num_pending_tx_sub_errors += 1;
                        datapoint_error!(
                            "searcher_pending_tx_sub_error",
                            ("errors", num_pending_tx_sub_errors, i64),
                            ("error_str", e.to_string(), String)
                        );
                    }
                }
            }
            Err(e) => {
                num_searcher_connection_errors += 1;
                datapoint_error!(
                    "searcher_connection_error",
                    ("errors", num_searcher_connection_errors, i64),
                    ("error_str", e.to_string(), String)
                );
            }
        }
    }
}
