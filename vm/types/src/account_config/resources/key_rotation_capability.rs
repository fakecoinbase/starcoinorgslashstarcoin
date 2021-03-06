// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{account_address::AccountAddress, account_config::constants::ACCOUNT_MODULE_NAME};
use move_core_types::move_resource::MoveResource;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct KeyRotationCapabilityResource {
    account_address: AccountAddress,
}

impl KeyRotationCapabilityResource {
    pub fn account_address(&self) -> &AccountAddress {
        &self.account_address
    }
}

impl MoveResource for KeyRotationCapabilityResource {
    const MODULE_NAME: &'static str = ACCOUNT_MODULE_NAME;
    const STRUCT_NAME: &'static str = "KeyRotationCapability";
}
