
<a name="0x1_Genesis"></a>

# Module `0x1::Genesis`

### Table of Contents

-  [Function `initialize`](#0x1_Genesis_initialize)



<a name="0x1_Genesis_initialize"></a>

## Function `initialize`



<pre><code><b>public</b> <b>fun</b> <a href="#0x1_Genesis_initialize">initialize</a>(publishing_option: vector&lt;u8&gt;, instruction_schedule: vector&lt;u8&gt;, native_schedule: vector&lt;u8&gt;, reward_delay: u64, uncle_rate_target: u64, epoch_time_target: u64, reward_half_epoch: u64, init_block_time_target: u64, block_difficulty_window: u64, min_time_target: u64, reward_per_uncle_percent: u64, max_uncles_per_block: u64, total_supply: u128, pre_mine_percent: u64, parent_hash: vector&lt;u8&gt;, association_auth_key: vector&lt;u8&gt;, genesis_auth_key: vector&lt;u8&gt;)
</code></pre>



<details>
<summary>Implementation</summary>


<pre><code><b>public</b> <b>fun</b> <a href="#0x1_Genesis_initialize">initialize</a>(publishing_option: vector&lt;u8&gt;, instruction_schedule: vector&lt;u8&gt;,
                      native_schedule: vector&lt;u8&gt;, reward_delay: u64,
                      uncle_rate_target:u64,epoch_time_target: u64,
                      reward_half_epoch: u64, init_block_time_target: u64,
                      block_difficulty_window: u64, min_time_target:u64,
                      reward_per_uncle_percent: u64, max_uncles_per_block:u64, total_supply: u128,
                      pre_mine_percent:u64, parent_hash: vector&lt;u8&gt;,
                      association_auth_key: vector&lt;u8&gt;, genesis_auth_key: vector&lt;u8&gt;,
){
     <b>assert</b>(<a href="Timestamp.md#0x1_Timestamp_is_genesis">Timestamp::is_genesis</a>(), 1);

     <b>let</b> dummy_auth_key_prefix = x"00000000000000000000000000000000";

     // create genesis account
     <b>let</b> genesis_account = <a href="Account.md#0x1_Account_create_genesis_account">Account::create_genesis_account</a>(<a href="CoreAddresses.md#0x1_CoreAddresses_GENESIS_ACCOUNT">CoreAddresses::GENESIS_ACCOUNT</a>(),<b>copy</b> dummy_auth_key_prefix);

     <a href="Block.md#0x1_Block_initialize">Block::initialize</a>(&genesis_account, parent_hash);

     // init config
     <a href="VMConfig.md#0x1_VMConfig_initialize">VMConfig::initialize</a>(&genesis_account, publishing_option, instruction_schedule, native_schedule);
     <a href="Version.md#0x1_Version_initialize">Version::initialize</a>(&genesis_account);

     <a href="TransactionTimeout.md#0x1_TransactionTimeout_initialize">TransactionTimeout::initialize</a>(&genesis_account);

     <a href="STC.md#0x1_STC_initialize">STC::initialize</a>(&genesis_account);
     <a href="Account.md#0x1_Account_accept_token">Account::accept_token</a>&lt;<a href="STC.md#0x1_STC">STC</a>&gt;(&genesis_account);

     <b>let</b> association = <a href="Account.md#0x1_Account_create_genesis_account">Account::create_genesis_account</a>(<a href="CoreAddresses.md#0x1_CoreAddresses_ASSOCIATION_ROOT_ADDRESS">CoreAddresses::ASSOCIATION_ROOT_ADDRESS</a>(), <b>copy</b> dummy_auth_key_prefix);
     <a href="Account.md#0x1_Account_accept_token">Account::accept_token</a>&lt;<a href="STC.md#0x1_STC">STC</a>&gt;(&association);

     <b>let</b> association_balance = total_supply * (pre_mine_percent <b>as</b> u128) / 100;
     <b>if</b> (association_balance &gt; 0) {
          <a href="Account.md#0x1_Account_mint_to_address">Account::mint_to_address</a>&lt;<a href="STC.md#0x1_STC">STC</a>&gt;(&genesis_account, <a href="Signer.md#0x1_Signer_address_of">Signer::address_of</a>(&association), association_balance);
     };
     <b>let</b> miner_reward_balance = total_supply - association_balance;
     <b>let</b> init_reward_per_epoch = miner_reward_balance / (reward_half_epoch * 2 <b>as</b> u128);
     <a href="Consensus.md#0x1_Consensus_initialize">Consensus::initialize</a>(&genesis_account,uncle_rate_target,epoch_time_target,reward_half_epoch, init_block_time_target, block_difficulty_window,
                             init_reward_per_epoch, reward_per_uncle_percent, min_time_target, max_uncles_per_block);

     <a href="BlockReward.md#0x1_BlockReward_initialize">BlockReward::initialize</a>(&genesis_account, miner_reward_balance, reward_delay);

     <a href="TransactionFee.md#0x1_TransactionFee_initialize">TransactionFee::initialize</a>(&genesis_account);
     //Grant stdlib maintainer <b>to</b> association
     <a href="PackageTxnManager.md#0x1_PackageTxnManager_grant_maintainer">PackageTxnManager::grant_maintainer</a>(&genesis_account, <a href="Signer.md#0x1_Signer_address_of">Signer::address_of</a>(&association));
     //TODO set stdlib upgrade strategy.

     // only dev network set genesis auth key.
     <b>if</b> (!<a href="Vector.md#0x1_Vector_is_empty">Vector::is_empty</a>(&genesis_auth_key)){
         <b>let</b> genesis_rotate_key_cap = <a href="Account.md#0x1_Account_extract_key_rotation_capability">Account::extract_key_rotation_capability</a>(&genesis_account);
         <a href="Account.md#0x1_Account_rotate_authentication_key">Account::rotate_authentication_key</a>(&genesis_rotate_key_cap, genesis_auth_key);
         <a href="Account.md#0x1_Account_restore_key_rotation_capability">Account::restore_key_rotation_capability</a>(genesis_rotate_key_cap);
     };

     <b>let</b> assoc_rotate_key_cap = <a href="Account.md#0x1_Account_extract_key_rotation_capability">Account::extract_key_rotation_capability</a>(&association);
     <a href="Account.md#0x1_Account_rotate_authentication_key">Account::rotate_authentication_key</a>(&assoc_rotate_key_cap, association_auth_key);
     <a href="Account.md#0x1_Account_restore_key_rotation_capability">Account::restore_key_rotation_capability</a>(assoc_rotate_key_cap);

     //Set <b>global</b> time, and <a href="Timestamp.md#0x1_Timestamp_is_genesis">Timestamp::is_genesis</a>() will <b>return</b> <b>false</b>.
     <a href="Timestamp.md#0x1_Timestamp_initialize">Timestamp::initialize</a>(&genesis_account);

     <a href="Account.md#0x1_Account_release_genesis_signer">Account::release_genesis_signer</a>(genesis_account);
     <a href="Account.md#0x1_Account_release_genesis_signer">Account::release_genesis_signer</a>(association);
}
</code></pre>



</details>
