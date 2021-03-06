// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::{ensure, format_err, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use starcoin_accumulator::node::{AccumulatorStoreType, ACCUMULATOR_PLACEHOLDER_HASH};
use starcoin_accumulator::{Accumulator, MerkleAccumulator};
use starcoin_chain::BlockChain;
use starcoin_config::{genesis_key_pair, ChainNetwork};
use starcoin_consensus::{argon::ArgonConsensus, dev::DevConsensus};
use starcoin_logger::prelude::*;
use starcoin_state_api::ChainState;
use starcoin_statedb::ChainStateDB;
use starcoin_storage::cache_storage::CacheStorage;
use starcoin_storage::storage::StorageInstance;
use starcoin_storage::{Storage, Store};
use starcoin_transaction_builder::{build_stdlib_package, StdLibOptions};
use starcoin_types::startup_info::StartupInfo;
use starcoin_types::transaction::TransactionInfo;
use starcoin_types::{block::Block, transaction::Transaction};
use starcoin_vm_types::account_config::CORE_CODE_ADDRESS;
use starcoin_vm_types::transaction::{
    RawUserTransaction, SignedUserTransaction, TransactionPayload,
};
use starcoin_vm_types::vm_status::KeptVMStatus;
use std::collections::HashMap;
use std::fmt::Display;
use std::fs::{create_dir_all, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use traits::{ChainReader, ConnectBlockResult, Consensus};

pub static GENESIS_FILE_NAME: &str = "genesis";
pub static GENESIS_GENERATED_DIR: &str = "generated";

const DEV_GENESIS_BYTES: &[u8] = std::include_bytes!("../generated/dev/genesis");
const HALLEY_GENESIS_BYTES: &[u8] = std::include_bytes!("../generated/halley/genesis");
const PROXIMA_GENESIS_BYTES: &[u8] = std::include_bytes!("../generated/proxima/genesis");
const MAIN_GENESIS_BYTES: &[u8] = std::include_bytes!("../generated/main/genesis");

pub static FRESH_GENESIS: Lazy<HashMap<ChainNetwork, Genesis>> = Lazy::new(|| {
    let mut genesis = HashMap::new();
    for net in ChainNetwork::networks() {
        genesis.insert(
            net,
            Genesis::build(net)
                .unwrap_or_else(|e| panic!("build genesis for {} fail: {:?}", net, e)),
        );
    }
    genesis
});

pub enum GenesisOpt {
    /// Load generated genesis
    Generated,
    /// Regenerate genesis
    Fresh,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct Genesis {
    block: Block,
}

impl Display for Genesis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Genesis {{")?;
        write!(f, "block: {:?}", self.block.header.id())?;
        write!(f, "}}")?;
        Ok(())
    }
}

impl Genesis {
    pub fn load_by_opt(option: GenesisOpt, net: ChainNetwork) -> Result<Self> {
        match option {
            GenesisOpt::Generated => Self::load_generated(net),
            GenesisOpt::Fresh => (&FRESH_GENESIS)
                .get(&net)
                .cloned()
                .ok_or_else(|| format_err!("Can not find genesis by net{:?}", net)),
        }
    }

    /// Load pre generated genesis.
    pub fn load(net: ChainNetwork) -> Result<Self> {
        Self::load_by_opt(GenesisOpt::Generated, net)
    }

    /// Build fresh genesis
    pub(crate) fn build(net: ChainNetwork) -> Result<Self> {
        debug!("Init genesis");
        let block = Self::build_genesis_block(net)?;
        assert_eq!(block.header().number(), 0);
        debug!("Genesis block id : {:?}", block.header().id());
        let genesis = Self { block };
        Ok(genesis)
    }

    fn build_genesis_block(net: ChainNetwork) -> Result<Block> {
        let chain_config = net.get_config();

        let txn = Self::build_genesis_transaction(net)?;

        let storage = Arc::new(Storage::new(StorageInstance::new_cache_instance(
            CacheStorage::new(),
        ))?);
        let chain_state_db = ChainStateDB::new(storage.clone(), None);

        let transaction_info = Self::execute_genesis_txn(&chain_state_db, txn.clone())?;

        let accumulator = MerkleAccumulator::new(
            *ACCUMULATOR_PLACEHOLDER_HASH,
            vec![],
            0,
            0,
            AccumulatorStoreType::Transaction,
            storage,
        )?;
        let txn_info_hash = transaction_info.id();

        let (accumulator_root, _) = accumulator.append(vec![txn_info_hash].as_slice())?;
        accumulator.flush()?;
        Ok(Block::genesis_block(
            chain_config.parent_hash,
            chain_config.timestamp,
            accumulator_root,
            transaction_info.state_root_hash(),
            chain_config.difficulty,
            chain_config.nonce,
            txn,
        ))
    }

    pub fn build_genesis_transaction(net: ChainNetwork) -> Result<SignedUserTransaction> {
        let package = build_stdlib_package(net, StdLibOptions::Staged, true)?;
        let txn = RawUserTransaction::new(
            CORE_CODE_ADDRESS,
            0,
            TransactionPayload::Package(package),
            0,
            0,
            1, // init to 1 to pass time check
        );
        let (genesis_private_key, genesis_public_key) = genesis_key_pair();
        let sign_txn = txn.sign(&genesis_private_key, genesis_public_key)?;
        Ok(sign_txn.into_inner())
    }

    pub fn execute_genesis_txn(
        chain_state: &dyn ChainState,
        txn: SignedUserTransaction,
    ) -> Result<TransactionInfo> {
        let txn = Transaction::UserTransaction(txn);
        let txn_hash = txn.id();

        let output = starcoin_executor::execute_transactions(chain_state.as_super(), vec![txn])?
            .pop()
            .expect("Execute output must exist.");
        let (write_set, events, gas_used, _, status) = output.into_inner();
        assert_eq!(gas_used, 0, "Genesis txn output's gas_used must be zero");
        let keep_status = status
            .status()
            .map_err(|e| format_err!("Genesis txn is discard by: {:?}", e))?;
        ensure!(
            keep_status == KeptVMStatus::Executed,
            "Genesis txn execute fail for: {:?}",
            keep_status
        );
        chain_state.apply_write_set(write_set)?;
        let state_root = chain_state.commit()?;
        chain_state.flush()?;
        Ok(TransactionInfo::new(
            txn_hash,
            state_root,
            events.as_slice(),
            gas_used,
            keep_status,
        ))
    }

    pub fn block(&self) -> &Block {
        &self.block
    }

    pub fn load_from_dir<P>(data_dir: P) -> Result<Option<Self>>
    where
        P: AsRef<Path>,
    {
        let genesis_file_path = data_dir.as_ref().join(GENESIS_FILE_NAME);
        if !genesis_file_path.exists() {
            return Ok(None);
        }
        let mut genesis_file = File::open(genesis_file_path)?;
        let mut content = vec![];
        genesis_file.read_to_end(&mut content)?;
        let genesis = scs::from_bytes(&content)?;
        Ok(Some(genesis))
    }

    fn genesis_bytes(net: ChainNetwork) -> &'static [u8] {
        match net {
            ChainNetwork::Dev => DEV_GENESIS_BYTES,
            ChainNetwork::Halley => HALLEY_GENESIS_BYTES,
            ChainNetwork::Proxima => PROXIMA_GENESIS_BYTES,
            ChainNetwork::Main => MAIN_GENESIS_BYTES,
        }
    }

    pub fn load_generated(net: ChainNetwork) -> Result<Self> {
        let bytes = Self::genesis_bytes(net);
        scs::from_bytes(bytes)
    }

    pub fn execute_genesis_block(
        self,
        net: ChainNetwork,
        storage: Arc<dyn Store>,
    ) -> Result<StartupInfo> {
        if net.is_dev() {
            self.execute_genesis_block_inner::<DevConsensus>(storage)
        } else {
            self.execute_genesis_block_inner::<ArgonConsensus>(storage)
        }
    }

    pub fn execute_genesis_block_inner<C>(self, storage: Arc<dyn Store>) -> Result<StartupInfo>
    where
        C: Consensus + 'static,
    {
        let Genesis { block } = self;
        let mut genesis_chain = BlockChain::<C>::init_empty_chain(storage.clone())?;
        if let ConnectBlockResult::SUCCESS = genesis_chain.apply_inner(block, true)? {
            let startup_info = StartupInfo::new(genesis_chain.current_header().id(), Vec::new());
            storage.save_startup_info(startup_info.clone())?;
            Ok(startup_info)
        } else {
            Err(format_err!("Apply genesis block failed."))
        }
    }

    pub fn save<P>(&self, data_dir: P) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let data_dir = data_dir.as_ref();
        if !data_dir.exists() {
            create_dir_all(data_dir)?;
        }
        let genesis_file = data_dir.join(GENESIS_FILE_NAME);
        let mut file = File::create(genesis_file)?;
        let contents = scs::to_bytes(self)?;
        file.write_all(&contents)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starcoin_crypto::HashValue;
    use starcoin_state_api::AccountStateReader;
    use starcoin_storage::block_info::BlockInfoStore;
    use starcoin_storage::cache_storage::CacheStorage;
    use starcoin_storage::storage::StorageInstance;
    use starcoin_storage::{BlockStore, IntoSuper, Storage};
    use starcoin_types::account_config::genesis_address;
    use starcoin_vm_types::account_config::association_address;
    use starcoin_vm_types::on_chain_config::{EpochResource, VMConfig, Version};

    #[stest::test]
    pub fn test_genesis_load() -> Result<()> {
        for net in ChainNetwork::networks() {
            Genesis::load(net)?;
        }
        Ok(())
    }

    #[stest::test]
    pub fn test_genesis() -> Result<()> {
        for net in ChainNetwork::networks() {
            do_test_genesis(net)?;
        }
        Ok(())
    }

    pub fn do_test_genesis(net: ChainNetwork) -> Result<()> {
        let temp_dir = starcoin_config::temp_path();
        let genesis = Genesis::build(net)?;
        debug!("build genesis {} for {:?}", genesis, net);
        genesis.save(temp_dir.as_ref())?;
        let genesis2 = Genesis::load_from_dir(temp_dir.as_ref())?;
        assert!(genesis2.is_some(), "load genesis fail.");
        let genesis2 = genesis2.unwrap();
        assert_eq!(genesis, genesis2, "genesis save and load different.");

        let storage = Arc::new(Storage::new(StorageInstance::new_cache_instance(
            CacheStorage::new(),
        ))?);
        let startup_info = genesis.execute_genesis_block(net, storage.clone())?;

        let storage2 = Arc::new(Storage::new(StorageInstance::new_cache_instance(
            CacheStorage::new(),
        ))?);
        let startup_info2 = genesis2.execute_genesis_block(net, storage2)?;

        assert_eq!(
            startup_info, startup_info2,
            "genesis execute startup info different."
        );
        let genesis_block = storage
            .get_block(startup_info.master)?
            .expect("Genesis block must exist.");
        let state_db = ChainStateDB::new(
            storage.clone().into_super_arc(),
            Some(genesis_block.header().state_root()),
        );
        let account_state_reader = AccountStateReader::new(&state_db);
        let account_resource = account_state_reader.get_account_resource(&association_address())?;
        assert!(
            account_resource.is_some(),
            "association account must exist in genesis state."
        );

        let vm_config = account_state_reader.get_on_chain_config::<VMConfig>()?;
        assert!(
            vm_config.is_some(),
            "VMConfig on_chain_config should exist."
        );

        let version = account_state_reader.get_on_chain_config::<Version>()?;
        assert!(version.is_some(), "Version on_chain_config should exist.");

        let block_info = storage
            .get_block_info(genesis_block.header().id())?
            .expect("Genesis block info must exist.");

        let txn_accumulator_info = block_info.get_txn_accumulator_info();
        let txn_accumulator = MerkleAccumulator::new(
            *txn_accumulator_info.get_accumulator_root(),
            txn_accumulator_info.get_frozen_subtree_roots().clone(),
            txn_accumulator_info.get_num_leaves(),
            txn_accumulator_info.get_num_nodes(),
            AccumulatorStoreType::Transaction,
            storage.clone().into_super_arc(),
        )?;
        //ensure block_accumulator can work.
        txn_accumulator.append(&[HashValue::random()])?;
        txn_accumulator.flush()?;

        let block_accumulator_info = block_info.get_block_accumulator_info();
        let block_accumulator = MerkleAccumulator::new(
            *block_accumulator_info.get_accumulator_root(),
            block_accumulator_info.get_frozen_subtree_roots().clone(),
            block_accumulator_info.get_num_leaves(),
            block_accumulator_info.get_num_nodes(),
            AccumulatorStoreType::Block,
            storage.into_super_arc(),
        )?;
        let hash = block_accumulator.get_leaf(0)?.expect("leaf 0 must exist.");
        assert_eq!(hash, block_info.block_id);
        //ensure block_accumulator can work.
        block_accumulator.append(&[HashValue::random()])?;
        block_accumulator.flush()?;

        let epoch = account_state_reader.get_resource::<EpochResource>(genesis_address())?;
        assert!(epoch.is_some(), "Epoch resource should exist.");

        Ok(())
    }
}
