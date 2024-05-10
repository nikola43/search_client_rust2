use std::{env, path::PathBuf, sync::Arc, time::Duration};

use clap::{Parser, Subcommand};
use env_logger::TimestampPrecision;
use futures_util::StreamExt;
use jito_protos::{
    convert::versioned_tx_from_packet,
    searcher::{
        mempool_subscription, searcher_service_client::SearcherServiceClient,
        ConnectedLeadersRegionedRequest, GetTipAccountsRequest, MempoolSubscription,
        NextScheduledLeaderRequest, PendingTxNotification, ProgramSubscriptionV0,
        SubscribeBundleResultsRequest, WriteLockedAccountSubscriptionV0,
    },
};
use jito_searcher_client::{
    get_searcher_client, send_bundle_with_confirmation, token_authenticator::ClientInterceptor,
};
use log::info;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signature, Signer},
    system_instruction::{self, transfer},
    transaction::{Transaction, VersionedTransaction},
};
use spl_memo::build_memo;
use std::str::FromStr;
use tokio::time::{sleep, timeout};
use tonic::{codegen::InterceptedService, transport::Channel, Streaming};
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// URL of the block engine.
    /// See: https://jito-labs.gitbook.io/mev/searcher-resources/block-engine#connection-details
    #[arg(long, env)]
    block_engine_url: String,

    /// Path to keypair file used to authenticate with the Jito Block Engine
    /// See: https://jito-labs.gitbook.io/mev/searcher-resources/getting-started#block-engine-api-key
    #[arg(long, env)]
    keypair_path: PathBuf,

    /// Comma-separated list of regions to request cross-region data from.
    /// If no region specified, then default to the currently connected block engine's region.
    /// Details: https://jito-labs.gitbook.io/mev/searcher-services/recommendations#cross-region
    /// Available regions: https://jito-labs.gitbook.io/mev/searcher-resources/block-engine#connection-details
    #[arg(long, env, value_delimiter = ',')]
    regions: Vec<String>,

    /// Subcommand to run
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print out information on the next scheduled leader
    NextScheduledLeader,

    /// Prints out information on connected leaders
    ConnectedLeaders,

    /// Prints out connected leaders with their leader slot percentage
    ConnectedLeadersInfo {
        #[clap(long, required = true)]
        rpc_url: String,
    },

    /// Prints out information about the tip accounts
    TipAccounts,

    /// Sends a 1 lamport bundle
    SendBundle {
        /// RPC URL
        #[clap(long, required = true)]
        rpc_url: String,
        /// Filepath to keypair that can afford the transaction payments with 1 lamport tip
        #[clap(long, required = true)]
        payer: PathBuf,
        /// Message you'd like the bundle to say
        #[clap(long, required = true)]
        message: String,
        /// Number of transactions in the bundle (must be <= 5)
        #[clap(long, required = true)]
        num_txs: usize,
        /// Amount of lamports to tip in each transaction
        #[clap(long, required = true)]
        lamports: u64,
        /// One of the tip accounts, see https://jito-foundation.gitbook.io/mev/mev-payment-and-distribution/on-chain-addresses
        #[clap(long, required = true)]
        tip_account: Pubkey,
    },
}

async fn print_next_leader_info(
    client: &mut SearcherServiceClient<InterceptedService<Channel, ClientInterceptor>>,
    regions: Vec<String>,
) {
    let next_leader = client
        .get_next_scheduled_leader(NextScheduledLeaderRequest { regions })
        .await
        .expect("gets next scheduled leader")
        .into_inner();
    println!(
        "next jito-solana slot in {} slots for leader {:?}",
        next_leader.next_leader_slot - next_leader.current_slot,
        next_leader.next_leader_identity
    );
}

#[tokio::main]
async fn main() {
    env_logger::builder()
        .format_timestamp(Some(TimestampPrecision::Micros))
        .init();

    let num_txs = 2;
    let lamports = 1000;
    let rpc_url = "https://api.mainnet-beta.solana.com";
    let block_engine_url = "https://amsterdam.mainnet.block-engine.jito.wtf";
    let keypair_path = "./jito-keypair.json";
    let keypair = Arc::new(read_keypair_file(keypair_path).expect("reads keypair at path"));
    let payer = "./my-keypair.json";
    let payer_keypair = read_keypair_file(&payer).expect("reads keypair at path");
    let regions = vec!["amsterdam".to_string()];

    let mut client = get_searcher_client(block_engine_url, &keypair)
        .await
        .expect("connects to searcher client");

    let tip_accounts = client
        .get_tip_accounts(GetTipAccountsRequest {})
        .await
        .expect("gets connected leaders")
        .into_inner();
    info!("{:?}", tip_accounts);

    let rpc_client =
        RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
    let balance = rpc_client
        .get_balance(&payer_keypair.pubkey())
        .await
        .expect("reads balance");

    info!(
        "payer public key: {:?} lamports: {balance:?}",
        payer_keypair.pubkey(),
    );

    let mut bundle_results_subscription = client
        .subscribe_bundle_results(SubscribeBundleResultsRequest {})
        .await
        .expect("subscribe to bundle results")
        .into_inner();

    // wait for jito-solana leader slot
    let mut is_leader_slot = false;
    while !is_leader_slot {
        let next_leader = client
            .get_next_scheduled_leader(NextScheduledLeaderRequest {
                regions: regions.clone(),
            })
            .await
            .expect("gets next scheduled leader")
            .into_inner();
        let num_slots = next_leader.next_leader_slot - next_leader.current_slot;
        is_leader_slot = num_slots <= 2;
        info!(
            "next jito leader slot in {num_slots} slots in {}",
            next_leader.next_leader_region
        );
        sleep(Duration::from_millis(500)).await;
    }

    let tip_accounts = tip_accounts.accounts;
    let tip_account = Pubkey::from_str(tip_accounts[0].as_str()).unwrap();
    let message = "jito bundle 0: hello world";

    let recipient_pubkey =
        Pubkey::from_str("GKxpQ3ZSMNSbCDs1RrqhxuTTUfn8xx6faDzTss38mkw3").unwrap();
    let lamports_to_send = 1_000; // 1 SOL

    // build + sign the transactions
    let blockhash = rpc_client
        .get_latest_blockhash()
        .await
        .expect("get blockhash");

    let transfer_instruction = system_instruction::transfer(
        &payer_keypair.pubkey(), // From (payer)
        &recipient_pubkey,       // To (recipient)
        lamports_to_send,        // Amount
    );

    let jito_tip_instruction = transfer(&payer_keypair.pubkey(), &tip_account, lamports);

    let mut transaction = Transaction::new_with_payer(
        &[transfer_instruction, jito_tip_instruction],
        Some(&payer_keypair.pubkey()),
    );
    transaction.sign(&[&payer_keypair], blockhash);
    let wire_transaction = bincode::serialize(&transaction).unwrap();
    let transaction_signature = transaction.signatures[0];

    println!("Sending transaction: {:?}", transaction_signature);

    let txs: &Vec<Signature> = &vec![transaction.signatures[0]];
    //let signatures= [wire_transaction];

    let vec_of_vecs: Vec<Vec<u8>> = vec![wire_transaction];
    let signatures: &[Vec<u8>] = &vec_of_vecs;

    /*

    pub async fn send_bundle_with_confirmation(
        bundle_signatures: &Vec<Signature>,
        transactions: &[Vec<u8>],
        rpc_client: &RpcClient,
        searcher_client: &mut SearcherServiceClient<InterceptedService<Channel, ClientInterceptor>>,
        bundle_results_subscription: &mut Streaming<BundleResult>,
    )


          bundle_signatures: &Vec<Signature>,
        transactions: &[Vec<u8>],
         */

    // let txs: Vec<_> = (0..num_txs)
    //     .map(|i| {
    //         VersionedTransaction::from(Transaction::new_signed_with_payer(
    //             &[
    //                 build_memo(format!("jito bundle {i}: {message}").as_bytes(), &[]),
    //                 transfer(&payer_keypair.pubkey(), &tip_account, lamports),
    //             ],
    //             Some(&payer_keypair.pubkey()),
    //             &[&payer_keypair],
    //             blockhash,
    //         ))
    //     })
    //     .collect();

    send_bundle_with_confirmation(
        &txs,
        &signatures,
        &rpc_client,
        &mut client,
        &mut bundle_results_subscription,
    )
    .await
    .expect("Sending bundle failed");
}

async fn print_packet_stream(
    client: &mut SearcherServiceClient<InterceptedService<Channel, ClientInterceptor>>,
    mut pending_transactions: Streaming<PendingTxNotification>,
    regions: Vec<String>,
) {
    loop {
        match timeout(Duration::from_secs(5), pending_transactions.next()).await {
            Ok(Some(Ok(notification))) => {
                let transactions: Vec<VersionedTransaction> = notification
                    .transactions
                    .iter()
                    .filter_map(versioned_tx_from_packet)
                    .collect();
                for tx in transactions {
                    info!("tx sig: {:?}", tx.signatures[0]);
                }
            }
            Ok(Some(Err(e))) => {
                info!("error from pending transaction stream: {e:?}");
                break;
            }
            Ok(None) => {
                info!("pending transaction stream closed");
                break;
            }
            Err(_) => {
                print_next_leader_info(client, regions.clone()).await;
            }
        }
    }
}
