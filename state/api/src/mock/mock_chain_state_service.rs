// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{ChainStateAsyncService, StateWithProof};
use anyhow::Result;
use starcoin_crypto::HashValue;
use starcoin_types::access_path::AccessPath;
use starcoin_types::account_address::AccountAddress;
use starcoin_types::account_state::AccountState;

//TODO implement Mock service
#[derive(Clone, Default)]
pub struct MockChainStateService {}

impl MockChainStateService {
    pub fn new() -> MockChainStateService {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChainStateAsyncService for MockChainStateService {
    async fn get(self, _access_path: AccessPath) -> Result<Option<Vec<u8>>> {
        unimplemented!()
    }

    async fn get_with_proof(self, _access_path: AccessPath) -> Result<StateWithProof> {
        unimplemented!()
    }

    async fn get_account_state(self, _address: AccountAddress) -> Result<Option<AccountState>> {
        unimplemented!()
    }

    async fn state_root(self) -> Result<HashValue> {
        unimplemented!()
    }
}
