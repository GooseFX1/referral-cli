use anchor_lang::prelude::AccountMeta;
use anchor_lang::AccountDeserialize;
use anyhow::Context;
use clap::{Parser, Subcommand};
use referral::accounts as referral_accounts;
use referral::instruction as referral_instructions;
use referral::InitializeReferralAccountParams;
use referral::InitializeReferralAccountWithNameParams;
use referral::REFERRAL_ATA_SEED;
use referral::REFERRAL_SEED;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::v0::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::system_program;
use solana_sdk::transaction::Transaction;
use solana_sdk::transaction::VersionedTransaction;
use std::collections::HashSet;
use std::str::FromStr;

mod utils;

#[derive(Debug, Parser)]
pub struct Opts {
    /// The cluster RPC url
    #[clap(long, env = "HTTP_URL")]
    http_url: String,

    /// The cluster WS url
    #[clap(long, env = "WS_URL")]
    ws_url: Option<String>,

    /// Keypair base58 string
    #[clap(long, env)]
    keypair: Option<String>,

    /// The project account key
    #[clap(long, env)]
    project: Option<Pubkey>,

    /// The referral program
    #[clap(
        long,
        env,
        default_value = "REFER4ZgmyYx9c6He5XfaTMiGfdLwRnkV4RPp9t9iF3"
    )]
    referral_program: Pubkey,

    /// Subcommand
    #[clap(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Action {
    /// Create a referral account, optionally wih a name
    CreateReferralAccount { name: Option<String> },
    /// Create token-accounts for a referral account
    CreateReferralTokenAccounts {
        /// The referral account key
        #[clap(long, env)]
        referral_account: Pubkey,
        /// Path to a json file containing a list of mints
        path: String,
    },
    /// Fetch, deserialize, and display a referral account
    FetchReferralAccount {
        /// The account to fetch
        account: Pubkey,
    },
}

/// Max number of addresses a LUT can contain
const MAX_LUT_SIZE: usize = 256;
/// How many accounts each init-token-account instruction needs
const INIT_REFERRAL_ATA_ACCOUNTS_LEN: usize = 7;
/// Max number of accounts that can fit in a legacy transaction
const MAX_LEGACY_ACCOUNTS: usize = 32;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv()?;
    let opts = Opts::parse();
    let rpc_client = RpcClient::new(opts.http_url.clone());
    let keypair = opts.keypair.map(|s| Keypair::from_base58_string(&s));

    match opts.command {
        Action::CreateReferralAccount { name } => {
            let keypair = keypair.context("keypair not set")?;
            let project = opts
                .project
                .context("no project specified for referral account creation")?;
            let referral_account = Keypair::new();
            let (data, accounts) = if let Some(name) = name {
                let referral_pda = Pubkey::find_program_address(
                    &[REFERRAL_SEED, project.as_ref(), name.as_bytes()],
                    &opts.referral_program,
                )
                .0;
                let data = anchor_lang::InstructionData::data(
                    &referral_instructions::InitializeReferralAccountWithName {
                        params: InitializeReferralAccountWithNameParams { name },
                    },
                );

                let accounts = anchor_lang::ToAccountMetas::to_account_metas(
                    &referral_accounts::InitializeReferralAccountWithName {
                        payer: keypair.pubkey(),
                        partner: keypair.pubkey(),
                        project,
                        referral_account: referral_pda,
                        system_program: solana_sdk::system_program::ID,
                    },
                    None,
                );

                (data, accounts)
            } else {
                let data = anchor_lang::InstructionData::data(
                    &referral_instructions::InitializeReferralAccount {
                        params: InitializeReferralAccountParams {},
                    },
                );

                let accounts = anchor_lang::ToAccountMetas::to_account_metas(
                    &referral_accounts::InitializeReferralAccount {
                        payer: keypair.pubkey(),
                        partner: keypair.pubkey(),
                        project,
                        referral_account: referral_account.pubkey(),
                        system_program: solana_sdk::system_program::ID,
                    },
                    None,
                );

                (data, accounts)
            };
            let instruction = Instruction::new_with_bytes(opts.referral_program, &data, accounts);

            let recent_hash = rpc_client.get_latest_blockhash().await?;
            let txn = Transaction::new_signed_with_payer(
                &[instruction],
                Some(&keypair.pubkey()),
                &vec![&keypair],
                recent_hash,
            );
            let signature = rpc_client
                .send_and_confirm_transaction_with_spinner_and_config(
                    &txn,
                    CommitmentConfig::confirmed(),
                    RpcSendTransactionConfig {
                        skip_preflight: true,
                        preflight_commitment: Some(rpc_client.commitment().commitment),
                        max_retries: Some(0),
                        ..RpcSendTransactionConfig::default()
                    },
                )
                .await?;
            println!("View confirmed txn at: https://solscan.io/tx/{}", signature);
        }
        Action::CreateReferralTokenAccounts {
            path,
            referral_account,
        } => {
            let mints = serde_json::from_str::<Vec<String>>(&std::fs::read_to_string(path)?)?
                .into_iter()
                .filter_map(|p| Pubkey::from_str(&p).ok())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let keypair = keypair.context("keypair not set")?;
            let project = opts
                .project
                .context("no project specified for referral token-account creation")?;

            let fits_legacy_transaction =
                mints.len() < MAX_LEGACY_ACCOUNTS / INIT_REFERRAL_ATA_ACCOUNTS_LEN;

            if fits_legacy_transaction {
                let mut instructions = Vec::with_capacity(mints.len());
                for mint in mints {
                    let (data, accounts) = create_referral_token_account_data_and_accounts(
                        keypair.pubkey(),
                        opts.referral_program,
                        mint,
                        project,
                        referral_account,
                    );
                    instructions.push(Instruction::new_with_bytes(
                        opts.referral_program,
                        &data,
                        accounts,
                    ));
                }
                let recent_hash = rpc_client.get_latest_blockhash().await?;
                let txn = Transaction::new_signed_with_payer(
                    &instructions,
                    Some(&keypair.pubkey()),
                    &vec![&keypair],
                    recent_hash,
                );
                let signature = rpc_client
                    .send_and_confirm_transaction_with_spinner_and_config(
                        &txn,
                        CommitmentConfig::confirmed(),
                        RpcSendTransactionConfig {
                            skip_preflight: true,
                            preflight_commitment: Some(rpc_client.commitment().commitment),
                            max_retries: Some(0),
                            ..RpcSendTransactionConfig::default()
                        },
                    )
                    .await?;
                println!("View confirmed txn at: https://solscan.io/tx/{}", signature);
            } else {
                for mints in mints.chunks(MAX_LUT_SIZE / INIT_REFERRAL_ATA_ACCOUNTS_LEN) {
                    // About 7 accounts per-instruction
                    let mut instructions = Vec::with_capacity(mints.len());
                    let mut extend_accounts = HashSet::new();

                    for mint in mints {
                        let (data, accounts) = create_referral_token_account_data_and_accounts(
                            keypair.pubkey(),
                            opts.referral_program,
                            *mint,
                            project,
                            referral_account,
                        );
                        extend_accounts.extend(accounts.iter().map(|meta| meta.pubkey));
                        instructions.push(Instruction::new_with_bytes(
                            opts.referral_program,
                            &data,
                            accounts,
                        ));
                    }

                    let lut = utils::create_and_extend_lookup_table(
                        &keypair,
                        &rpc_client,
                        extend_accounts,
                        None,
                    )
                    .await?;
                    let lut_account = utils::fetch_address_lookup_table(&rpc_client, lut).await?;
                    let blockhash = rpc_client.get_latest_blockhash().await?;
                    let message = Message::try_compile(
                        &keypair.pubkey(),
                        &instructions,
                        &[lut_account],
                        blockhash,
                    )?;
                    let transaction = VersionedTransaction::try_new(
                        solana_sdk::message::VersionedMessage::V0(message),
                        &[&keypair],
                    )?;
                    let signature = rpc_client
                        .send_and_confirm_transaction_with_spinner_and_config(
                            &transaction,
                            CommitmentConfig::confirmed(),
                            RpcSendTransactionConfig {
                                skip_preflight: true,
                                preflight_commitment: Some(rpc_client.commitment().commitment),
                                max_retries: Some(0),
                                ..RpcSendTransactionConfig::default()
                            },
                        )
                        .await?;
                    println!("View confirmed txn at: https://solscan.io/tx/{}", signature);
                }
            }
        }
        Action::FetchReferralAccount { account } => {
            let data = rpc_client.get_account_data(&account).await?;
            let account = referral::ReferralAccount::try_deserialize(&mut &data[..])?;

            #[derive(Debug)]
            #[allow(dead_code)]
            struct ReferralAccount {
                pub partner: Pubkey,
                pub project: Pubkey,
                pub share_bps: u16,
                pub name: Option<String>,
            }
            impl From<referral::ReferralAccount> for ReferralAccount {
                fn from(value: referral::ReferralAccount) -> Self {
                    ReferralAccount {
                        partner: value.partner,
                        project: value.project,
                        share_bps: value.share_bps,
                        name: value.name,
                    }
                }
            }
            println!("account: {:#?}", ReferralAccount::from(account));
        }
    }

    Ok(())
}

fn create_referral_token_account_data_and_accounts(
    payer: Pubkey,
    program: Pubkey,
    mint: Pubkey,
    project: Pubkey,
    referral_account: Pubkey,
) -> (Vec<u8>, Vec<AccountMeta>) {
    let referral_token_account = Pubkey::find_program_address(
        &[REFERRAL_ATA_SEED, referral_account.as_ref(), mint.as_ref()],
        &program,
    )
    .0;
    let data =
        anchor_lang::InstructionData::data(&referral_instructions::InitializeReferralTokenAccount);
    let accounts = anchor_lang::ToAccountMetas::to_account_metas(
        &referral_accounts::InitializeReferralTokenAccount {
            payer,
            project,
            referral_account,
            referral_token_account,
            mint,
            system_program: system_program::ID,
            token_program: anchor_spl::token::ID, // todo: token-2022 support
        },
        None,
    );

    (data, accounts)
}
