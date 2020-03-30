// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::state::CliState;
use crate::StarcoinOpt;
use anyhow::{bail, format_err, Result};
use scmd::{CommandAction, ExecContext};
use starcoin_executor::executor::Executor;
use starcoin_executor::TransactionExecutor;
use starcoin_rpc_client::RemoteStateReader;
use starcoin_state_api::AccountStateReader;
use starcoin_types::account_address::AccountAddress;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "transfer")]
pub struct TransferOpt {
    #[structopt(short = "f")]
    /// if from is absent, use default account.
    from: Option<AccountAddress>,
    #[structopt(short = "t")]
    to: AccountAddress,
    #[structopt(short = "k")]
    /// if to account not exist on chain, must set this key prefix.
    auth_key_prefix: Option<String>,
    #[structopt(short = "v")]
    amount: u64,
}

pub struct TransferCommand {}

impl CommandAction for TransferCommand {
    type State = CliState;
    type GlobalOpt = StarcoinOpt;
    type Opt = TransferOpt;

    fn run(&self, ctx: &ExecContext<Self::State, Self::GlobalOpt, Self::Opt>) -> Result<()> {
        let client = ctx.state().client();
        let opt = ctx.opt();
        let sender = match opt.from {
            Some(from) => client.account_get(from)?.ok_or(format_err!(
                "Can not find WalletAccount by address: {}",
                from
            ))?,
            None => {
                //TODO supported default account.
                bail!("Please input from account.");
            }
        };
        let to = opt.to;
        //TODO check to is onchain
        let to_auth_key_prefix = opt.auth_key_prefix.clone().unwrap_or("".to_string());
        let to_auth_key_prefix = hex::decode(to_auth_key_prefix)?;
        let chain_state_reader = RemoteStateReader::new(client);
        let account_state_reader = AccountStateReader::new(&chain_state_reader);
        let account_resource = account_state_reader
            .get_account_resource(sender.address())?
            .ok_or(format_err!(
                "Can not find account on chain by address:{}",
                sender.address()
            ))?;
        let raw_txn = Executor::build_transfer_txn(
            sender.account.address,
            sender.get_auth_key().prefix().to_vec(),
            to,
            to_auth_key_prefix,
            account_resource.sequence_number(),
            opt.amount,
        );
        let txn = client.account_sign_txn(raw_txn)?;
        println!("Submit txn: {:?}", txn);
        client.submit_transaction(txn)?;
        Ok(())
    }
}