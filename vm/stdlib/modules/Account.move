address 0x1 {

// The module for the account resource that governs every account
module Account {
    use 0x1::Event;
    use 0x1::Hash;
    use 0x1::LCS;
    use 0x1::Token::{Self, Token};
    use 0x1::Vector;
    use 0x1::Signer;
    use 0x1::Timestamp;
    use 0x1::Option::{Self, Option};
    use 0x1::SignedInteger64::{Self};
    use 0x1::TransactionFee;
    use 0x1::CoreAddresses;

    // Every account has a Account::Account resource
    resource struct Account {
        // The current authentication key.
        // This can be different than the key used to create the account
        authentication_key: vector<u8>,
        // A `withdrawal_capability` allows whoever holds this capability
        // to withdraw from the account. At the time of account creation
        // this capability is stored in this option. It can later be
        // "extracted" from this field via `extract_withdraw_capability`,
        // and can also be restored via `restore_withdraw_capability`.
        withdrawal_capability: Option<WithdrawCapability>,
        // A `key_rotation_capability` allows whoever holds this capability
        // the ability to rotate the authentication key for the account. At
        // the time of account creation this capability is stored in this
        // option. It can later be "extracted" from this field via
        // `extract_key_rotation_capability`, and can also be restored via
        // `restore_key_rotation_capability`.
        key_rotation_capability: Option<KeyRotationCapability>,
        // Event handle for received event
        received_events: Event::EventHandle<ReceivedPaymentEvent>,
        // Event handle for sent event
        sent_events: Event::EventHandle<SentPaymentEvent>,
        // The current sequence number.
        // Incremented by one each time a transaction is submitted
        sequence_number: u64,
    }

    // A resource that holds the tokens stored in this account
    resource struct Balance<TokenType> {
        token: Token<TokenType>,
    }

    // The holder of WithdrawCapability for account_address can withdraw Token from
    // account_address/Account::Account/balance.
    // There is at most one WithdrawCapability in existence for a given address.
    resource struct WithdrawCapability {
        account_address: address,
    }

    // The holder of KeyRotationCapability for account_address can rotate the authentication key for
    // account_address (i.e., write to account_address/Account::Account/authentication_key).
    // There is at most one KeyRotationCapability in existence for a given address.
    resource struct KeyRotationCapability {
        account_address: address,
    }

    // Message for sent events
    struct SentPaymentEvent {
        // The amount of Token<TokenType> sent
        amount: u128,
        // The code symbol for the token that was sent
        token_code: vector<u8>,
        // The address that was paid
        payee: address,
        // Metadata associated with the payment
        metadata: vector<u8>,
    }

    // Message for received events
    struct ReceivedPaymentEvent {
        // The amount of Token<TokenType> received
        amount: u128,
        // The code symbol for the token that was received
        token_code: vector<u8>,
        // The address that sent the token
        payer: address,
        // Metadata associated with the payment
        metadata: vector<u8>,
    }

    // Deposits the `to_deposit` token into the `payee`'s account balance
    public fun deposit<TokenType>(account: &signer, payee: address, to_deposit: Token<TokenType>)
    acquires Account, Balance {
        // Since we don't have vector<u8> literals in the source language at
        // the moment.
        deposit_with_metadata(account, payee, to_deposit, x"", x"")
    }

    // Deposits the `to_deposit` token into the sender's account balance
    public fun deposit_to_sender<TokenType>(account: &signer, to_deposit: Token<TokenType>)
    acquires Account, Balance {
        deposit(account, Signer::address_of(account), to_deposit)
    }

    // Deposits the `to_deposit` token into the `payee`'s account balance with the attached `metadata`
    public fun deposit_with_metadata<TokenType>(account: &signer,
        payee: address,
        to_deposit: Token<TokenType>,
        metadata: vector<u8>,
        metadata_signature: vector<u8>
    ) acquires Account, Balance {
        deposit_with_sender_and_metadata(
            payee,
            Signer::address_of(account),
            to_deposit,
            metadata,
            metadata_signature
        );
    }

    // Deposits the `to_deposit` token into the `payee`'s account balance with the attached `metadata` and
    // sender address
    fun deposit_with_sender_and_metadata<TokenType>(
        payee: address,
        sender: address,
        to_deposit: Token<TokenType>,
        metadata: vector<u8>,
        _metadata_signature: vector<u8>
    ) acquires Account, Balance {
        // Check that the `to_deposit` token is non-zero
        let deposit_value = Token::value(&to_deposit);
        assert(deposit_value > 0, 7);

        //TODO check signature
        //assert(Vector::length(&metadata_signature) == 64, 9001);
        // cryptographic check of signature validity
        //assert(
        //    Signature::ed25519_verify(
        //        metadata_signature,
        //        VASP::travel_rule_public_key(payee),
        //        copy metadata
        //    ),
        //    9002, // TODO: proper error code
        //);

        let token_code = Token::token_code<TokenType>();

        // Load the sender's account
        let sender_account_ref = borrow_global_mut<Account>(sender);
        // Log a sent event
        Event::emit_event<SentPaymentEvent>(
            &mut sender_account_ref.sent_events,
            SentPaymentEvent {
                amount: deposit_value,
                token_code: (copy token_code),
                payee: payee,
                metadata: *&metadata
            },
        );

        // Load the payee's account
        let payee_account_ref = borrow_global_mut<Account>(payee);
        let payee_balance = borrow_global_mut<Balance<TokenType>>(payee);
        // Deposit the `to_deposit` token
        Token::deposit(&mut payee_balance.token, to_deposit);
        // Log a received event
        Event::emit_event<ReceivedPaymentEvent>(
            &mut payee_account_ref.received_events,
            ReceivedPaymentEvent {
                amount: deposit_value,
                token_code: token_code,
                payer: sender,
                metadata: metadata
            }
        );
    }

    // mint_to_address can only be called by accounts with MintCapability
    // and those accounts will be charged for gas. If those accounts don't have enough gas to pay
    // for the transaction cost they will fail minting.
    // However those account can also mint to themselves so that is a decent workaround
    public fun mint_to_address<TokenType>(
        account: &signer,
        payee: address,
        amount: u128
    ) acquires Account, Balance {
        // Mint and deposit the token
        deposit(account, payee, Token::mint<TokenType>(account, amount));
    }

    // Helper to withdraw `amount` from the given account balance and return the withdrawn Token<TokenType>
    fun withdraw_from_balance<TokenType>(_addr: address, balance: &mut Balance<TokenType>, amount: u128): Token<TokenType>{
        Token::withdraw(&mut balance.token, amount)
    }

    // Withdraw `amount` Token<TokenType> from the transaction sender's account balance
    public fun withdraw_from_sender<TokenType>(account: &signer, amount: u128): Token<TokenType>
    acquires Account, Balance {
        let sender_addr = Signer::address_of(account);
        let sender_balance = borrow_global_mut<Balance<TokenType>>(sender_addr);
        // The sender_addr has delegated the privilege to withdraw from her account elsewhere--abort.
        assert(!delegated_withdraw_capability(sender_addr), 11);
        // The sender_addr has retained her withdrawal privileges--proceed.
        withdraw_from_balance<TokenType>(sender_addr, sender_balance, amount)
    }

    // Withdraw `amount` Token<TokenType> from the account under cap.account_address
    public fun withdraw_with_capability<TokenType>(
        cap: &WithdrawCapability, amount: u128
    ): Token<TokenType> acquires Balance {
        let balance = borrow_global_mut<Balance<TokenType>>(cap.account_address);
        withdraw_from_balance<TokenType>(cap.account_address, balance , amount)
    }

    // Return a unique capability granting permission to withdraw from the sender's account balance.
    public fun extract_withdraw_capability(
        sender: &signer
    ): WithdrawCapability acquires Account {
        let sender_addr = Signer::address_of(sender);
        // Abort if we already extracted the unique withdraw capability for this account.
        assert(!delegated_withdraw_capability(sender_addr), 11);
        let account = borrow_global_mut<Account>(sender_addr);
        Option::extract(&mut account.withdrawal_capability)
    }

     // Return the withdraw capability to the account it originally came from
     public fun restore_withdraw_capability(cap: WithdrawCapability)
        acquires Account {
            let account = borrow_global_mut<Account>(cap.account_address);
            Option::fill(&mut account.withdrawal_capability, cap)
     }

    // Withdraws `amount` Token<TokenType> using the passed in WithdrawCapability, and deposits it
    // into the `payee`'s account balance. Creates the `payee` account if it doesn't exist.
    public fun pay_from_capability<TokenType>(
        payee: address,
        cap: &WithdrawCapability,
        amount: u128,
        metadata: vector<u8>,
        metadata_signature: vector<u8>
    ) acquires Account, Balance {
        deposit_with_sender_and_metadata<TokenType>(
            payee,
            *&cap.account_address,
            withdraw_with_capability(cap, amount),
            metadata,
            metadata_signature
        );
    }

    // Withdraw `amount` Token<TokenType> from the transaction sender's
    // account balance and send the token to the `payee` address with the
    // attached `metadata` Creates the `payee` account if it does not exist
    public fun pay_from_sender_with_metadata<TokenType>(
        account: &signer,
        payee: address,
        amount: u128,
        metadata: vector<u8>,
        metadata_signature: vector<u8>
    ) acquires Account, Balance {
        deposit_with_metadata<TokenType>(
            account,
            payee,
            withdraw_from_sender(account, amount),
            metadata,
            metadata_signature
        );
    }

    // Withdraw `amount` Token<TokenType> from the transaction sender's
    // account balance  and send the token to the `payee` address
    // Creates the `payee` account if it does not exist
    public fun pay_from_sender<TokenType>(
        account: &signer,
        payee: address,
        amount: u128
    ) acquires Account, Balance {
        pay_from_sender_with_metadata<TokenType>(account, payee, amount, x"", x"");
    }

    fun rotate_authentication_key_for_account(account: &mut Account, new_authentication_key: vector<u8>) {
      // Don't allow rotating to clearly invalid key
      assert(Vector::length(&new_authentication_key) == 32, 12);
      account.authentication_key = new_authentication_key;
    }

    // Rotate the authentication key for the account under cap.account_address
    public fun rotate_authentication_key(
        cap: &KeyRotationCapability,
        new_authentication_key: vector<u8>,
    ) acquires Account  {
        let sender_account_resource = borrow_global_mut<Account>(cap.account_address);
        // Don't allow rotating to clearly invalid key
        assert(Vector::length(&new_authentication_key) == 32, 12);
        sender_account_resource.authentication_key = new_authentication_key;
    }

    // Return a unique capability granting permission to rotate the sender's authentication key
    public fun extract_key_rotation_capability(account: &signer): KeyRotationCapability
    acquires Account {
        let account_address = Signer::address_of(account);
        // Abort if we already extracted the unique key rotation capability for this account.
        assert(!delegated_key_rotation_capability(account_address), 11);
        let account = borrow_global_mut<Account>(account_address);
        Option::extract(&mut account.key_rotation_capability)
    }

    // Return the key rotation capability to the account it originally came from
    public fun restore_key_rotation_capability(cap: KeyRotationCapability)
    acquires Account {
        let account = borrow_global_mut<Account>(cap.account_address);
        Option::fill(&mut account.key_rotation_capability, cap)
    }

    // Create an account at `new_account_address` with authentication key
    /// `auth_key_prefix` | `new_account_address`
    // TODO: can we get rid of this? the main thing this does is create an account without an
    // Token and return signer. (which is just needed to avoid circular dep issues in Genesis)
    public fun create_genesis_account(
        new_account_address: address,
        auth_key_prefix: vector<u8>
    ) :signer {
        assert(Timestamp::is_genesis(), 1);
        let new_account = create_signer(new_account_address);
        Event::publish_generator(&new_account);
        make_account(&new_account, auth_key_prefix);
        new_account
    }

    // Release genesis account signer
    public fun release_genesis_signer(genesis_account: signer){
        destroy_signer(genesis_account);
    }

    // Creates a new account at `fresh_address` with a balance of zero and authentication
    // key `auth_key_prefix` | `fresh_address`.
    // Creating an account at address 0x1 will cause runtime failure as it is a
    // reserved address for the MoveVM.
    public fun create_account<TokenType>(fresh_address: address, auth_key_prefix: vector<u8>){
        let new_account = create_signer(fresh_address);
        Event::publish_generator(&new_account);
        make_account(&new_account, auth_key_prefix);
        Self::accept_token<TokenType>(&new_account);
        destroy_signer(new_account);
    }

    fun make_account(
        new_account: &signer,
        auth_key_prefix: vector<u8>,
    ) {
        let authentication_key = auth_key_prefix;
        let new_account_addr = Signer::address_of(new_account);
        Vector::append(&mut authentication_key, LCS::to_bytes(&new_account_addr));
        assert(Vector::length(&authentication_key) == 32, 12);
        move_to(new_account, Account {
              authentication_key,
              withdrawal_capability: Option::some(
                  WithdrawCapability {
                      account_address: new_account_addr
              }),
              key_rotation_capability: Option::some(
                  KeyRotationCapability {
                      account_address: new_account_addr
              }),
              received_events: Event::new_event_handle<ReceivedPaymentEvent>(new_account),
              sent_events: Event::new_event_handle<SentPaymentEvent>(new_account),
              sequence_number: 0,
        });
    }

    native fun create_signer(addr: address): signer;
    native fun destroy_signer(sig: signer);

    // Helper to return the u128 value of the `balance` for `account`
    fun balance_for<TokenType>(balance: &Balance<TokenType>): u128 {
        Token::value<TokenType>(&balance.token)
    }

    // Return the current TokenType balance of the account at `addr`.
    public fun balance<TokenType>(addr: address): u128 acquires Balance {
        balance_for(borrow_global<Balance<TokenType>>(addr))
    }

    // Add a balance of `Token` type to the sending account.
    public fun accept_token<TokenType>(account: &signer) {
        move_to(account, Balance<TokenType>{ token: Token::zero<TokenType>() })
    }

    // Return whether the account at `addr` accepts `Token` type tokens
    public fun is_accepts_token<TokenType>(addr: address): bool {
        exists<Balance<TokenType>>(addr)
    }

    // Helper to return the sequence number field for given `account`
    fun sequence_number_for_account(account: &Account): u64 {
        account.sequence_number
    }

    // Return the current sequence number at `addr`
    public fun sequence_number(addr: address): u64 acquires Account {
        sequence_number_for_account(borrow_global<Account>(addr))
    }

    // Return the authentication key for this account
    public fun authentication_key(addr: address): vector<u8> acquires Account {
        *&borrow_global<Account>(addr).authentication_key
    }

    // Return true if the account at `addr` has delegated its key rotation capability
    public fun delegated_key_rotation_capability(addr: address): bool
    acquires Account {
        Option::is_none(&borrow_global<Account>(addr).key_rotation_capability)
    }

    // Return true if the account at `addr` has delegated its withdraw capability
    public fun delegated_withdraw_capability(addr: address): bool
    acquires Account {
        Option::is_none(&borrow_global<Account>(addr).withdrawal_capability)
    }

    // Return a reference to the address associated with the given withdraw capability
    public fun withdraw_capability_address(cap: &WithdrawCapability): &address {
        &cap.account_address
    }

    // Return a reference to the address associated with the given key rotation capability
    public fun key_rotation_capability_address(cap: &KeyRotationCapability): &address {
        &cap.account_address
    }

    // Checks if an account exists at `check_addr`
    public fun exists_at(check_addr: address): bool {
        exists<Account>(check_addr)
    }


    // The prologue is invoked at the beginning of every transaction
    // It verifies:
    // - The account's auth key matches the transaction's public key
    // - That the account has enough balance to pay for all of the gas
    // - That the sequence number matches the transaction's sequence key
    public fun txn_prologue<TokenType>(
        account: &signer,
        txn_sender: address,
        txn_sequence_number: u64,
        txn_public_key: vector<u8>,
        txn_gas_price: u64,
        txn_max_gas_units: u64,
    ) acquires Account, Balance {
        assert(Signer::address_of(account) == CoreAddresses::GENESIS_ACCOUNT(), 33);

        // FUTURE: Make these error codes sequential
        // Verify that the transaction sender's account exists
        assert(exists_at(txn_sender), 4);

        // Load the transaction sender's account
        let sender_account = borrow_global_mut<Account>(txn_sender);

        // Check that the hash of the transaction's public key matches the account's auth key
        assert(
            Hash::sha3_256(txn_public_key) == *&sender_account.authentication_key,
            2
        );

        // Check that the account has enough balance for all of the gas
        let max_transaction_fee = txn_gas_price * txn_max_gas_units;
        let balance_amount = balance<TokenType>(txn_sender);
        assert(balance_amount >= (max_transaction_fee as u128), 6);

        // Check that the transaction sequence number matches the sequence number of the account
        assert(txn_sequence_number >= sender_account.sequence_number, 2);
        assert(txn_sequence_number == sender_account.sequence_number, 3);
    }

    // The epilogue is invoked at the end of transactions.
    // It collects gas and bumps the sequence number
    public fun txn_epilogue<TokenType>(
        account: &signer,
        txn_sender: address,
        txn_sequence_number: u64,
        txn_gas_price: u64,
        txn_max_gas_units: u64,
        gas_units_remaining: u64,
        state_cost_amount: u64,
        cost_is_negative: bool,
    ) acquires Account, Balance {
        assert(Signer::address_of(account) == CoreAddresses::GENESIS_ACCOUNT(), 33);

        // Load the transaction sender's account and balance resources
        let sender_account = borrow_global_mut<Account>(txn_sender);
        let sender_balance = borrow_global_mut<Balance<TokenType>>(txn_sender);

        // Charge for gas
        let transaction_fee_amount =(txn_gas_price * (txn_max_gas_units - gas_units_remaining) as u128);
        assert(
            balance_for(sender_balance) >= transaction_fee_amount,
            6
        );

        let cost = SignedInteger64::create_from_raw_value(state_cost_amount, cost_is_negative);
        assert(
            SignedInteger64::get_value(cost) >= 0, 7
         );

        // Bump the sequence number
        sender_account.sequence_number = txn_sequence_number + 1;

        if (transaction_fee_amount > 0) {
            let transaction_fee = withdraw_from_balance(
                    Signer::address_of(account),
                    sender_balance,
                    transaction_fee_amount
            );
            TransactionFee::pay_fee(transaction_fee);
        };
    }
}

}
