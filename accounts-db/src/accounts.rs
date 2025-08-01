use {
    crate::{
        account_overrides::AccountOverrides,
        account_rent_state::{check_rent_state_with_account, RentState},
        accounts_db::{
            AccountShrinkThreshold, AccountsAddRootTiming, AccountsDb, AccountsDbConfig,
            IncludeSlotInHash, LoadHint, LoadedAccount, ScanStorageResult,
            VerifyAccountsHashAndLamportsConfig, ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
        },
        accounts_index::{
            AccountSecondaryIndexes, IndexKey, ScanConfig, ScanError, ScanResult, ZeroLamport,
        },
        accounts_update_notifier_interface::AccountsUpdateNotifier,
        ancestors::Ancestors,
        blockhash_queue::BlockhashQueue,
        nonce_info::{NonceFull, NonceInfo},
        rent_collector::RentCollector,
        rent_debits::RentDebits,
        storable_accounts::StorableAccounts,
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_results::{TransactionCheckResult, TransactionExecutionResult},
    },
    dashmap::DashMap,
    itertools::Itertools,
    log::*,
    solana_program_runtime::{
        compute_budget::{self, ComputeBudget},
        loaded_programs::LoadedProgramsForTxBatch,
    },
    solana_sdk::{
        account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
        account_utils::StateMut,
        address_lookup_table::{self, error::AddressLookupError, state::AddressLookupTable},
        bpf_loader_upgradeable::{self, UpgradeableLoaderState},
        clock::{BankId, Slot},
        feature_set::{
            self, add_set_tx_loaded_accounts_data_size_instruction,
            include_loaded_accounts_data_size_in_fee_calculation,
            remove_congestion_multiplier_from_fee_calculation, remove_deprecated_request_unit_ix,
            simplify_writable_program_account_check, FeatureSet,
        },
        fee::FeeStructure,
        genesis_config::ClusterType,
        message::{
            v0::{LoadedAddresses, MessageAddressTableLookup},
            SanitizedMessage,
        },
        native_loader,
        nonce::{
            state::{DurableNonce, Versions as NonceVersions},
            State as NonceState,
        },
        pubkey::Pubkey,
        saturating_add_assign,
        slot_hashes::SlotHashes,
        sysvar::{self, instructions::construct_instructions_data},
        transaction::{Result, SanitizedTransaction, TransactionAccountLocks, TransactionError},
        transaction_context::{IndexOfAccount, TransactionAccount},
    },
    solana_system_program::{get_system_account_kind, SystemAccountKind},
    std::{
        cmp::Reverse,
        collections::{
            hash_map::{self, Entry},
            BinaryHeap, HashMap, HashSet,
        },
        num::NonZeroUsize,
        ops::RangeBounds,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex,
        },
    },
};

pub type PubkeyAccountSlot = (Pubkey, AccountSharedData, Slot);

#[derive(Debug, Default, AbiExample)]
pub struct AccountLocks {
    write_locks: HashSet<Pubkey>,
    readonly_locks: HashMap<Pubkey, u64>,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum RewardInterval {
    /// the slot within the epoch is INSIDE the reward distribution interval
    InsideInterval,
    /// the slot within the epoch is OUTSIDE the reward distribution interval
    OutsideInterval,
}

impl AccountLocks {
    fn is_locked_readonly(&self, key: &Pubkey) -> bool {
        self.readonly_locks
            .get(key)
            .map_or(false, |count| *count > 0)
    }

    fn is_locked_write(&self, key: &Pubkey) -> bool {
        self.write_locks.contains(key)
    }

    fn insert_new_readonly(&mut self, key: &Pubkey) {
        assert!(self.readonly_locks.insert(*key, 1).is_none());
    }

    fn lock_readonly(&mut self, key: &Pubkey) -> bool {
        self.readonly_locks.get_mut(key).map_or(false, |count| {
            *count += 1;
            true
        })
    }

    fn unlock_readonly(&mut self, key: &Pubkey) {
        if let hash_map::Entry::Occupied(mut occupied_entry) = self.readonly_locks.entry(*key) {
            let count = occupied_entry.get_mut();
            *count -= 1;
            if *count == 0 {
                occupied_entry.remove_entry();
            }
        }
    }

    fn unlock_write(&mut self, key: &Pubkey) {
        self.write_locks.remove(key);
    }
}

/// This structure handles synchronization for db
#[derive(Debug, AbiExample)]
pub struct Accounts {
    /// Single global AccountsDb
    pub accounts_db: Arc<AccountsDb>,

    /// set of read-only and writable accounts which are currently
    /// being processed by banking/replay threads
    pub(crate) account_locks: Mutex<AccountLocks>,
}

// for the load instructions
pub type TransactionRent = u64;
pub type TransactionProgramIndices = Vec<Vec<IndexOfAccount>>;
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct LoadedTransaction {
    pub accounts: Vec<TransactionAccount>,
    pub program_indices: TransactionProgramIndices,
    pub rent: TransactionRent,
    pub rent_debits: RentDebits,
}

pub type TransactionLoadResult = (Result<LoadedTransaction>, Option<NonceFull>);

pub enum AccountAddressFilter {
    Exclude, // exclude all addresses matching the filter
    Include, // only include addresses matching the filter
}

pub fn construct_instructions_account_ex(message: &SanitizedMessage) -> AccountSharedData {
    AccountSharedData::from(Account {
        data: construct_instructions_data(&message.decompile_instructions()),
        owner: sysvar::id(),
        ..Account::default()
    })
}

impl Accounts {
    pub fn default_for_tests() -> Self {
        Self::new_empty(AccountsDb::default_for_tests())
    }

    pub fn new_with_config_for_tests(
        paths: Vec<PathBuf>,
        cluster_type: &ClusterType,
        account_indexes: AccountSecondaryIndexes,
        shrink_ratio: AccountShrinkThreshold,
    ) -> Self {
        Self::new_with_config(
            paths,
            cluster_type,
            account_indexes,
            shrink_ratio,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
    }

    pub fn new_with_config_for_benches(
        paths: Vec<PathBuf>,
        cluster_type: &ClusterType,
        account_indexes: AccountSecondaryIndexes,
        shrink_ratio: AccountShrinkThreshold,
    ) -> Self {
        Self::new_with_config(
            paths,
            cluster_type,
            account_indexes,
            shrink_ratio,
            Some(ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS),
            None,
            Arc::default(),
        )
    }

    pub fn new_with_config(
        paths: Vec<PathBuf>,
        cluster_type: &ClusterType,
        account_indexes: AccountSecondaryIndexes,
        shrink_ratio: AccountShrinkThreshold,
        accounts_db_config: Option<AccountsDbConfig>,
        accounts_update_notifier: Option<AccountsUpdateNotifier>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        Self::new_empty(AccountsDb::new_with_config(
            paths,
            cluster_type,
            account_indexes,
            shrink_ratio,
            accounts_db_config,
            accounts_update_notifier,
            exit,
        ))
    }

    pub fn new_empty(accounts_db: AccountsDb) -> Self {
        Self::new(Arc::new(accounts_db))
    }

    pub fn new(accounts_db: Arc<AccountsDb>) -> Self {
        Self {
            accounts_db,
            account_locks: Mutex::new(AccountLocks::default()),
        }
    }

    fn construct_instructions_account(message: &SanitizedMessage) -> AccountSharedData {
        AccountSharedData::from(Account {
            data: construct_instructions_data(&message.decompile_instructions()),
            owner: sysvar::id(),
            ..Account::default()
        })
    }

    /// If feature `cap_transaction_accounts_data_size` is active, total accounts data a
    /// transaction can load is limited to
    ///   if `set_tx_loaded_accounts_data_size` instruction is not activated or not used, then
    ///     default value of 64MiB to not break anyone in Mainnet-beta today
    ///   else
    ///     user requested loaded accounts size.
    ///     Note, requesting zero bytes will result transaction error
    fn get_requested_loaded_accounts_data_size_limit(
        tx: &SanitizedTransaction,
        feature_set: &FeatureSet,
    ) -> Result<Option<NonZeroUsize>> {
        if feature_set.is_active(&feature_set::cap_transaction_accounts_data_size::id()) {
            let mut compute_budget =
                ComputeBudget::new(compute_budget::MAX_COMPUTE_UNIT_LIMIT as u64);
            let _process_transaction_result = compute_budget.process_instructions(
                tx.message().program_instructions_iter(),
                !feature_set.is_active(&remove_deprecated_request_unit_ix::id()),
                feature_set.is_active(&add_set_tx_loaded_accounts_data_size_instruction::id()),
            );
            // sanitize against setting size limit to zero
            NonZeroUsize::new(compute_budget.loaded_accounts_data_size_limit).map_or(
                Err(TransactionError::InvalidLoadedAccountsDataSizeLimit),
                |v| Ok(Some(v)),
            )
        } else {
            // feature not activated, no loaded accounts data limit imposed.
            Ok(None)
        }
    }

    /// Accumulate loaded account data size into `accumulated_accounts_data_size`.
    /// Returns TransactionErr::MaxLoadedAccountsDataSizeExceeded if
    /// `requested_loaded_accounts_data_size_limit` is specified and
    /// `accumulated_accounts_data_size` exceeds it.
    fn accumulate_and_check_loaded_account_data_size(
        accumulated_loaded_accounts_data_size: &mut usize,
        account_data_size: usize,
        requested_loaded_accounts_data_size_limit: Option<NonZeroUsize>,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Result<()> {
        if let Some(requested_loaded_accounts_data_size) = requested_loaded_accounts_data_size_limit
        {
            saturating_add_assign!(*accumulated_loaded_accounts_data_size, account_data_size);
            if *accumulated_loaded_accounts_data_size > requested_loaded_accounts_data_size.get() {
                error_counters.max_loaded_accounts_data_size_exceeded += 1;
                Err(TransactionError::MaxLoadedAccountsDataSizeExceeded)
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    fn account_shared_data_from_program(
        key: &Pubkey,
        program_accounts: &HashMap<Pubkey, (&Pubkey, u64)>,
    ) -> Result<AccountSharedData> {
        // It's an executable program account. The program is already loaded in the cache.
        // So the account data is not needed. Return a dummy AccountSharedData with meta
        // information.
        let mut program_account = AccountSharedData::default();
        let (program_owner, _count) = program_accounts
            .get(key)
            .ok_or(TransactionError::AccountNotFound)?;
        program_account.set_owner(**program_owner);
        program_account.set_executable(true);
        Ok(program_account)
    }

    #[allow(clippy::too_many_arguments)]
    fn load_transaction_accounts(
        &self,
        ancestors: &Ancestors,
        tx: &SanitizedTransaction,
        fee: u64,
        error_counters: &mut TransactionErrorMetrics,
        rent_collector: &RentCollector,
        feature_set: &FeatureSet,
        account_overrides: Option<&AccountOverrides>,
        reward_interval: RewardInterval,
        program_accounts: &HashMap<Pubkey, (&Pubkey, u64)>,
        loaded_programs: &LoadedProgramsForTxBatch,
    ) -> Result<LoadedTransaction> {
        let in_reward_interval = reward_interval == RewardInterval::InsideInterval;

        // NOTE: this check will never fail because `tx` is sanitized
        if tx.signatures().is_empty() && fee != 0 {
            return Err(TransactionError::MissingSignatureForFee);
        }

        // There is no way to predict what program will execute without an error
        // If a fee can pay for execution then the program will be scheduled
        let mut validated_fee_payer = false;
        let mut tx_rent: TransactionRent = 0;
        let message = tx.message();
        let account_keys = message.account_keys();
        let mut accounts_found = Vec::with_capacity(account_keys.len());
        let mut account_deps = Vec::with_capacity(account_keys.len());
        let mut rent_debits = RentDebits::default();

        let set_exempt_rent_epoch_max =
            feature_set.is_active(&solana_sdk::feature_set::set_exempt_rent_epoch_max::id());

        let requested_loaded_accounts_data_size_limit =
            Self::get_requested_loaded_accounts_data_size_limit(tx, feature_set)?;
        let mut accumulated_accounts_data_size: usize = 0;

        let instruction_accounts = message
            .instructions()
            .iter()
            .flat_map(|instruction| &instruction.accounts)
            .unique()
            .collect::<Vec<&u8>>();

        let mut accounts = account_keys
            .iter()
            .enumerate()
            .map(|(i, key)| {
                let mut account_found = true;
                #[allow(clippy::collapsible_else_if)]
                let account = if solana_sdk::sysvar::instructions::check_id(key) {
                    Self::construct_instructions_account(message)
                } else {
                    let instruction_account = u8::try_from(i)
                        .map(|i| instruction_accounts.contains(&&i))
                        .unwrap_or(false);
                    let (account_size, mut account, rent) = if let Some(account_override) =
                        account_overrides.and_then(|overrides| overrides.get(key))
                    {
                        (account_override.data().len(), account_override.clone(), 0)
                    } else if let Some(program) = (feature_set
                        .is_active(&simplify_writable_program_account_check::id())
                        && !instruction_account
                        && !message.is_writable(i))
                    .then_some(())
                    .and_then(|_| loaded_programs.find(key))
                    {
                        // This condition block does special handling for accounts that are passed
                        // as instruction account to any of the instructions in the transaction.
                        // It's been noticed that some programs are reading other program accounts
                        // (that are passed to the program as instruction accounts). So such accounts
                        // are needed to be loaded even though corresponding compiled program may
                        // already be present in the cache.
                        Self::account_shared_data_from_program(key, program_accounts)
                            .map(|program_account| (program.account_size, program_account, 0))?
                    } else {
                        self.accounts_db
                            .load_with_fixed_root(ancestors, key)
                            .map(|(mut account, _)| {
                                if message.is_writable(i) {
                                    let rent_due = rent_collector
                                        .collect_from_existing_account(
                                            key,
                                            &mut account,
                                            self.accounts_db.filler_account_suffix.as_ref(),
                                            set_exempt_rent_epoch_max,
                                        )
                                        .rent_amount;
                                    (account.data().len(), account, rent_due)
                                } else {
                                    (account.data().len(), account, 0)
                                }
                            })
                            .unwrap_or_else(|| {
                                account_found = false;
                                let mut default_account = AccountSharedData::default();
                                if set_exempt_rent_epoch_max {
                                    // All new accounts must be rent-exempt (enforced in Bank::execute_loaded_transaction).
                                    // Currently, rent collection sets rent_epoch to u64::MAX, but initializing the account
                                    // with this field already set would allow us to skip rent collection for these accounts.
                                    default_account.set_rent_epoch(u64::MAX);
                                }
                                (default_account.data().len(), default_account, 0)
                            })
                    };
                    Self::accumulate_and_check_loaded_account_data_size(
                        &mut accumulated_accounts_data_size,
                        account_size,
                        requested_loaded_accounts_data_size_limit,
                        error_counters,
                    )?;

                    if !validated_fee_payer && message.is_non_loader_key(i) {
                        if i != 0 {
                            warn!("Payer index should be 0! {:?}", tx);
                        }

                        Self::validate_fee_payer(
                            key,
                            &mut account,
                            i as IndexOfAccount,
                            error_counters,
                            rent_collector,
                            feature_set,
                            fee,
                        )?;

                        validated_fee_payer = true;
                    }

                    if !feature_set.is_active(&simplify_writable_program_account_check::id()) {
                        if bpf_loader_upgradeable::check_id(account.owner()) {
                            if message.is_writable(i) && !message.is_upgradeable_loader_present() {
                                error_counters.invalid_writable_account += 1;
                                return Err(TransactionError::InvalidWritableAccount);
                            }

                            if account.executable() {
                                // The upgradeable loader requires the derived ProgramData account
                                if let Ok(UpgradeableLoaderState::Program {
                                    programdata_address,
                                }) = account.state()
                                {
                                    if self
                                        .accounts_db
                                        .load_with_fixed_root(ancestors, &programdata_address)
                                        .is_none()
                                    {
                                        error_counters.account_not_found += 1;
                                        return Err(TransactionError::ProgramAccountNotFound);
                                    }
                                } else {
                                    error_counters.invalid_program_for_execution += 1;
                                    return Err(TransactionError::InvalidProgramForExecution);
                                }
                            }
                        } else if account.executable() && message.is_writable(i) {
                            error_counters.invalid_writable_account += 1;
                            return Err(TransactionError::InvalidWritableAccount);
                        }
                    }

                    if in_reward_interval
                        && message.is_writable(i)
                        && solana_stake_program::check_id(account.owner())
                    {
                        error_counters.program_execution_temporarily_restricted += 1;
                        return Err(TransactionError::ProgramExecutionTemporarilyRestricted {
                            account_index: i as u8,
                        });
                    }

                    tx_rent += rent;
                    rent_debits.insert(key, rent, account.lamports());

                    account
                };

                accounts_found.push(account_found);
                Ok((*key, account))
            })
            .collect::<Result<Vec<_>>>()?;

        if !validated_fee_payer {
            error_counters.account_not_found += 1;
            return Err(TransactionError::AccountNotFound);
        }

        // Appends the account_deps at the end of the accounts,
        // this way they can be accessed in a uniform way.
        // At places where only the accounts are needed,
        // the account_deps are truncated using e.g:
        // accounts.iter().take(message.account_keys.len())
        accounts.append(&mut account_deps);

        let disable_builtin_loader_ownership_chains =
            feature_set.is_active(&feature_set::disable_builtin_loader_ownership_chains::ID);
        let builtins_start_index = accounts.len();
        let program_indices = message
            .instructions()
            .iter()
            .map(|instruction| {
                let mut account_indices = Vec::new();
                let mut program_index = instruction.program_id_index as usize;
                for _ in 0..5 {
                    let (program_id, program_account) = accounts
                        .get(program_index)
                        .ok_or(TransactionError::ProgramAccountNotFound)?;
                    let account_found = accounts_found.get(program_index).unwrap_or(&true);
                    if native_loader::check_id(program_id) {
                        return Ok(account_indices);
                    }
                    if !account_found {
                        error_counters.account_not_found += 1;
                        return Err(TransactionError::ProgramAccountNotFound);
                    }
                    if !program_account.executable() {
                        error_counters.invalid_program_for_execution += 1;
                        return Err(TransactionError::InvalidProgramForExecution);
                    }
                    account_indices.insert(0, program_index as IndexOfAccount);
                    let owner_id = program_account.owner();
                    if native_loader::check_id(owner_id) {
                        return Ok(account_indices);
                    }
                    program_index = if let Some(owner_index) = accounts
                        .get(builtins_start_index..)
                        .ok_or(TransactionError::ProgramAccountNotFound)?
                        .iter()
                        .position(|(key, _)| key == owner_id)
                    {
                        builtins_start_index.saturating_add(owner_index)
                    } else {
                        let owner_index = accounts.len();
                        if let Some((owner_account, _)) =
                            self.accounts_db.load_with_fixed_root(ancestors, owner_id)
                        {
                            if disable_builtin_loader_ownership_chains
                                && !native_loader::check_id(owner_account.owner())
                                || !owner_account.executable()
                            {
                                error_counters.invalid_program_for_execution += 1;
                                return Err(TransactionError::InvalidProgramForExecution);
                            }
                            Self::accumulate_and_check_loaded_account_data_size(
                                &mut accumulated_accounts_data_size,
                                owner_account.data().len(),
                                requested_loaded_accounts_data_size_limit,
                                error_counters,
                            )?;
                            accounts.push((*owner_id, owner_account));
                        } else {
                            error_counters.account_not_found += 1;
                            return Err(TransactionError::ProgramAccountNotFound);
                        }
                        owner_index
                    };
                    if disable_builtin_loader_ownership_chains {
                        account_indices.insert(0, program_index as IndexOfAccount);
                        return Ok(account_indices);
                    }
                }
                error_counters.call_chain_too_deep += 1;
                Err(TransactionError::CallChainTooDeep)
            })
            .collect::<Result<Vec<Vec<IndexOfAccount>>>>()?;

        Ok(LoadedTransaction {
            accounts,
            program_indices,
            rent: tx_rent,
            rent_debits,
        })
    }

    fn validate_fee_payer(
        payer_address: &Pubkey,
        payer_account: &mut AccountSharedData,
        payer_index: IndexOfAccount,
        error_counters: &mut TransactionErrorMetrics,
        rent_collector: &RentCollector,
        feature_set: &FeatureSet,
        fee: u64,
    ) -> Result<()> {
        if payer_account.lamports() == 0 {
            error_counters.account_not_found += 1;
            return Err(TransactionError::AccountNotFound);
        }
        let min_balance = match get_system_account_kind(payer_account).ok_or_else(|| {
            error_counters.invalid_account_for_fee += 1;
            TransactionError::InvalidAccountForFee
        })? {
            SystemAccountKind::System => 0,
            SystemAccountKind::Nonce => {
                // Should we ever allow a fees charge to zero a nonce account's
                // balance. The state MUST be set to uninitialized in that case
                rent_collector.rent.minimum_balance(NonceState::size())
            }
        };

        // allow collapsible-else-if to make removing the feature gate safer once activated
        #[allow(clippy::collapsible_else_if)]
        if feature_set.is_active(&feature_set::checked_arithmetic_in_fee_validation::id()) {
            payer_account
                .lamports()
                .checked_sub(min_balance)
                .and_then(|v| v.checked_sub(fee))
                .ok_or_else(|| {
                    error_counters.insufficient_funds += 1;
                    TransactionError::InsufficientFundsForFee
                })?;
        } else {
            if payer_account.lamports() < fee + min_balance {
                error_counters.insufficient_funds += 1;
                return Err(TransactionError::InsufficientFundsForFee);
            }
        }

        let payer_pre_rent_state = RentState::from_account(payer_account, &rent_collector.rent);
        payer_account
            .checked_sub_lamports(fee)
            .map_err(|_| TransactionError::InsufficientFundsForFee)?;

        let payer_post_rent_state = RentState::from_account(payer_account, &rent_collector.rent);
        check_rent_state_with_account(
            &payer_pre_rent_state,
            &payer_post_rent_state,
            payer_address,
            payer_account,
            payer_index,
        )
    }

    /// Returns a hash map of executable program accounts (program accounts that are not writable
    /// in the given transactions), and their owners, for the transactions with a valid
    /// blockhash or nonce.
    pub fn filter_executable_program_accounts<'a>(
        &self,
        ancestors: &Ancestors,
        txs: &[SanitizedTransaction],
        lock_results: &mut [TransactionCheckResult],
        program_owners: &'a [Pubkey],
        hash_queue: &BlockhashQueue,
    ) -> HashMap<Pubkey, (&'a Pubkey, u64)> {
        let mut result: HashMap<Pubkey, (&'a Pubkey, u64)> = HashMap::new();
        lock_results.iter_mut().zip(txs).for_each(|etx| {
            if let ((Ok(()), nonce), tx) = etx {
                if nonce
                    .as_ref()
                    .map(|nonce| nonce.lamports_per_signature())
                    .unwrap_or_else(|| {
                        hash_queue.get_lamports_per_signature(tx.message().recent_blockhash())
                    })
                    .is_some()
                {
                    tx.message()
                        .account_keys()
                        .iter()
                        .for_each(|key| match result.entry(*key) {
                            Entry::Occupied(mut entry) => {
                                let (_, count) = entry.get_mut();
                                saturating_add_assign!(*count, 1);
                            }
                            Entry::Vacant(entry) => {
                                if let Ok(index) = self.accounts_db.account_matches_owners(
                                    ancestors,
                                    key,
                                    program_owners,
                                ) {
                                    program_owners
                                        .get(index)
                                        .map(|owner| entry.insert((owner, 1)));
                                }
                            }
                        });
                } else {
                    // If the transaction's nonce account was not valid, and blockhash is not found,
                    // the transaction will fail to process. Let's not load any programs from the
                    // transaction, and update the status of the transaction.
                    *etx.0 = (Err(TransactionError::BlockhashNotFound), None);
                }
            }
        });
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_accounts(
        &self,
        ancestors: &Ancestors,
        txs: &[SanitizedTransaction],
        lock_results: Vec<TransactionCheckResult>,
        hash_queue: &BlockhashQueue,
        error_counters: &mut TransactionErrorMetrics,
        rent_collector: &RentCollector,
        feature_set: &FeatureSet,
        fee_structure: &FeeStructure,
        account_overrides: Option<&AccountOverrides>,
        in_reward_interval: RewardInterval,
        program_accounts: &HashMap<Pubkey, (&Pubkey, u64)>,
        loaded_programs: &LoadedProgramsForTxBatch,
    ) -> Vec<TransactionLoadResult> {
        txs.iter()
            .zip(lock_results)
            .map(|etx| match etx {
                (tx, (Ok(()), nonce)) => {
                    let lamports_per_signature = nonce
                        .as_ref()
                        .map(|nonce| nonce.lamports_per_signature())
                        .unwrap_or_else(|| {
                            hash_queue.get_lamports_per_signature(tx.message().recent_blockhash())
                        });
                    let fee = if let Some(lamports_per_signature) = lamports_per_signature {
                        fee_structure.calculate_fee(
                            tx.message(),
                            lamports_per_signature,
                            &ComputeBudget::fee_budget_limits(tx.message().program_instructions_iter(), feature_set),
                            feature_set.is_active(&remove_congestion_multiplier_from_fee_calculation::id()),
                            feature_set.is_active(&include_loaded_accounts_data_size_in_fee_calculation::id()),
                        )
                    } else {
                        return (Err(TransactionError::BlockhashNotFound), None);
                    };

                    let loaded_transaction = match self.load_transaction_accounts(
                        ancestors,
                        tx,
                        fee,
                        error_counters,
                        rent_collector,
                        feature_set,
                        account_overrides,
                        in_reward_interval,
                        program_accounts,
                        loaded_programs,
                    ) {
                        Ok(loaded_transaction) => loaded_transaction,
                        Err(e) => return (Err(e), None),
                    };

                    // Update nonce with fee-subtracted accounts
                    let nonce = if let Some(nonce) = nonce {
                        match NonceFull::from_partial(
                            nonce,
                            tx.message(),
                            &loaded_transaction.accounts,
                            &loaded_transaction.rent_debits,
                        ) {
                            Ok(nonce) => Some(nonce),
                            Err(e) => return (Err(e), None),
                        }
                    } else {
                        None
                    };

                    (Ok(loaded_transaction), nonce)
                }
                (_, (Err(e), _nonce)) => (Err(e), None),
            })
            .collect()
    }

    pub fn load_lookup_table_addresses(
        &self,
        ancestors: &Ancestors,
        address_table_lookup: &MessageAddressTableLookup,
        slot_hashes: &SlotHashes,
    ) -> std::result::Result<LoadedAddresses, AddressLookupError> {
        let table_account = self
            .accounts_db
            .load_with_fixed_root(ancestors, &address_table_lookup.account_key)
            .map(|(account, _rent)| account)
            .ok_or(AddressLookupError::LookupTableAccountNotFound)?;

        if table_account.owner() == &address_lookup_table::program::id() {
            let current_slot = ancestors.max_slot();
            let lookup_table = AddressLookupTable::deserialize(table_account.data())
                .map_err(|_ix_err| AddressLookupError::InvalidAccountData)?;

            Ok(LoadedAddresses {
                writable: lookup_table.lookup(
                    current_slot,
                    &address_table_lookup.writable_indexes,
                    slot_hashes,
                )?,
                readonly: lookup_table.lookup(
                    current_slot,
                    &address_table_lookup.readonly_indexes,
                    slot_hashes,
                )?,
            })
        } else {
            Err(AddressLookupError::InvalidAccountOwner)
        }
    }

    /// Slow because lock is held for 1 operation instead of many
    /// This always returns None for zero-lamport accounts.
    fn load_slow(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
        load_hint: LoadHint,
    ) -> Option<(AccountSharedData, Slot)> {
        self.accounts_db.load(ancestors, pubkey, load_hint)
    }

    pub fn load_with_fixed_root(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
    ) -> Option<(AccountSharedData, Slot)> {
        self.load_slow(ancestors, pubkey, LoadHint::FixedMaxRoot)
    }

    pub fn load_without_fixed_root(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
    ) -> Option<(AccountSharedData, Slot)> {
        self.load_slow(ancestors, pubkey, LoadHint::Unspecified)
    }

    /// scans underlying accounts_db for this delta (slot) with a map function
    ///   from LoadedAccount to B
    /// returns only the latest/current version of B for this slot
    pub fn scan_slot<F, B>(&self, slot: Slot, func: F) -> Vec<B>
    where
        F: Fn(LoadedAccount) -> Option<B> + Send + Sync,
        B: Sync + Send + Default + std::cmp::Eq,
    {
        let scan_result = self.accounts_db.scan_account_storage(
            slot,
            |loaded_account: LoadedAccount| {
                // Cache only has one version per key, don't need to worry about versioning
                func(loaded_account)
            },
            |accum: &DashMap<Pubkey, B>, loaded_account: LoadedAccount| {
                let loaded_account_pubkey = *loaded_account.pubkey();
                if let Some(val) = func(loaded_account) {
                    accum.insert(loaded_account_pubkey, val);
                }
            },
        );

        match scan_result {
            ScanStorageResult::Cached(cached_result) => cached_result,
            ScanStorageResult::Stored(stored_result) => stored_result
                .into_iter()
                .map(|(_pubkey, val)| val)
                .collect(),
        }
    }

    /// Returns all the accounts from `slot`
    ///
    /// If `program_id` is `Some`, filter the results to those whose owner matches `program_id`
    pub fn load_by_program_slot(
        &self,
        slot: Slot,
        program_id: Option<&Pubkey>,
    ) -> Vec<TransactionAccount> {
        self.scan_slot(slot, |stored_account| {
            program_id
                .map(|program_id| program_id == stored_account.owner())
                .unwrap_or(true)
                .then(|| (*stored_account.pubkey(), stored_account.take_account()))
        })
    }

    pub fn load_largest_accounts(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
        num: usize,
        filter_by_address: &HashSet<Pubkey>,
        filter: AccountAddressFilter,
    ) -> ScanResult<Vec<(Pubkey, u64)>> {
        if num == 0 {
            return Ok(vec![]);
        }
        let mut account_balances = BinaryHeap::new();
        self.accounts_db.scan_accounts(
            ancestors,
            bank_id,
            |option| {
                if let Some((pubkey, account, _slot)) = option {
                    if account.lamports() == 0 {
                        return;
                    }
                    let contains_address = filter_by_address.contains(pubkey);
                    let collect = match filter {
                        AccountAddressFilter::Exclude => !contains_address,
                        AccountAddressFilter::Include => contains_address,
                    };
                    if !collect {
                        return;
                    }
                    if account_balances.len() == num {
                        let Reverse(entry) = account_balances
                            .peek()
                            .expect("BinaryHeap::peek should succeed when len > 0");
                        if *entry >= (account.lamports(), *pubkey) {
                            return;
                        }
                        account_balances.pop();
                    }
                    account_balances.push(Reverse((account.lamports(), *pubkey)));
                }
            },
            &ScanConfig::default(),
        )?;
        Ok(account_balances
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse((balance, pubkey))| (pubkey, balance))
            .collect())
    }

    /// Only called from startup or test code.
    #[must_use]
    pub fn verify_accounts_hash_and_lamports(
        &self,
        slot: Slot,
        total_lamports: u64,
        base: Option<(Slot, /*capitalization*/ u64)>,
        config: VerifyAccountsHashAndLamportsConfig,
    ) -> bool {
        if let Err(err) =
            self.accounts_db
                .verify_accounts_hash_and_lamports(slot, total_lamports, base, config)
        {
            warn!("verify_accounts_hash failed: {err:?}, slot: {slot}");
            false
        } else {
            true
        }
    }

    pub fn is_loadable(lamports: u64) -> bool {
        // Don't ever load zero lamport accounts into runtime because
        // the existence of zero-lamport accounts are never deterministic!!
        lamports > 0
    }

    fn load_while_filtering<F: Fn(&AccountSharedData) -> bool>(
        collector: &mut Vec<TransactionAccount>,
        some_account_tuple: Option<(&Pubkey, AccountSharedData, Slot)>,
        filter: F,
    ) {
        if let Some(mapped_account_tuple) = some_account_tuple
            .filter(|(_, account, _)| Self::is_loadable(account.lamports()) && filter(account))
            .map(|(pubkey, account, _slot)| (*pubkey, account))
        {
            collector.push(mapped_account_tuple)
        }
    }

    fn load_with_slot(
        collector: &mut Vec<PubkeyAccountSlot>,
        some_account_tuple: Option<(&Pubkey, AccountSharedData, Slot)>,
    ) {
        if let Some(mapped_account_tuple) = some_account_tuple
            .filter(|(_, account, _)| Self::is_loadable(account.lamports()))
            .map(|(pubkey, account, slot)| (*pubkey, account, slot))
        {
            collector.push(mapped_account_tuple)
        }
    }

    pub fn load_by_program(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
        program_id: &Pubkey,
        config: &ScanConfig,
    ) -> ScanResult<Vec<TransactionAccount>> {
        let mut collector = Vec::new();
        self.accounts_db
            .scan_accounts(
                ancestors,
                bank_id,
                |some_account_tuple| {
                    Self::load_while_filtering(&mut collector, some_account_tuple, |account| {
                        account.owner() == program_id
                    })
                },
                config,
            )
            .map(|_| collector)
    }

    pub fn load_by_program_with_filter<F: Fn(&AccountSharedData) -> bool>(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
        program_id: &Pubkey,
        filter: F,
        config: &ScanConfig,
    ) -> ScanResult<Vec<TransactionAccount>> {
        let mut collector = Vec::new();
        self.accounts_db
            .scan_accounts(
                ancestors,
                bank_id,
                |some_account_tuple| {
                    Self::load_while_filtering(&mut collector, some_account_tuple, |account| {
                        account.owner() == program_id && filter(account)
                    })
                },
                config,
            )
            .map(|_| collector)
    }

    fn calc_scan_result_size(account: &AccountSharedData) -> usize {
        account.data().len()
            + std::mem::size_of::<AccountSharedData>()
            + std::mem::size_of::<Pubkey>()
    }

    /// Accumulate size of (pubkey + account) into sum.
    /// Return true iff sum > 'byte_limit_for_scan'
    fn accumulate_and_check_scan_result_size(
        sum: &AtomicUsize,
        account: &AccountSharedData,
        byte_limit_for_scan: &Option<usize>,
    ) -> bool {
        if let Some(byte_limit_for_scan) = byte_limit_for_scan.as_ref() {
            let added = Self::calc_scan_result_size(account);
            sum.fetch_add(added, Ordering::Relaxed)
                .saturating_add(added)
                > *byte_limit_for_scan
        } else {
            false
        }
    }

    fn maybe_abort_scan(
        result: ScanResult<Vec<TransactionAccount>>,
        config: &ScanConfig,
    ) -> ScanResult<Vec<TransactionAccount>> {
        if config.is_aborted() {
            ScanResult::Err(ScanError::Aborted(
                "The accumulated scan results exceeded the limit".to_string(),
            ))
        } else {
            result
        }
    }

    pub fn load_by_index_key_with_filter<F: Fn(&AccountSharedData) -> bool>(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
        index_key: &IndexKey,
        filter: F,
        config: &ScanConfig,
        byte_limit_for_scan: Option<usize>,
    ) -> ScanResult<Vec<TransactionAccount>> {
        let sum = AtomicUsize::default();
        let config = config.recreate_with_abort();
        let mut collector = Vec::new();
        let result = self
            .accounts_db
            .index_scan_accounts(
                ancestors,
                bank_id,
                *index_key,
                |some_account_tuple| {
                    Self::load_while_filtering(&mut collector, some_account_tuple, |account| {
                        let use_account = filter(account);
                        if use_account
                            && Self::accumulate_and_check_scan_result_size(
                                &sum,
                                account,
                                &byte_limit_for_scan,
                            )
                        {
                            // total size of results exceeds size limit, so abort scan
                            config.abort();
                        }
                        use_account
                    });
                },
                &config,
            )
            .map(|_| collector);
        Self::maybe_abort_scan(result, &config)
    }

    pub fn account_indexes_include_key(&self, key: &Pubkey) -> bool {
        self.accounts_db.account_indexes.include_key(key)
    }

    pub fn load_all(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
    ) -> ScanResult<Vec<PubkeyAccountSlot>> {
        let mut collector = Vec::new();
        self.accounts_db
            .scan_accounts(
                ancestors,
                bank_id,
                |some_account_tuple| {
                    if let Some((pubkey, account, slot)) = some_account_tuple
                        .filter(|(_, account, _)| Self::is_loadable(account.lamports()))
                    {
                        collector.push((*pubkey, account, slot))
                    }
                },
                &ScanConfig::default(),
            )
            .map(|_| collector)
    }

    pub fn scan_all<F>(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
        scan_func: F,
    ) -> ScanResult<()>
    where
        F: FnMut(Option<(&Pubkey, AccountSharedData, Slot)>),
    {
        self.accounts_db
            .scan_accounts(ancestors, bank_id, scan_func, &ScanConfig::default())
    }

    pub fn hold_range_in_memory<R>(
        &self,
        range: &R,
        start_holding: bool,
        thread_pool: &rayon::ThreadPool,
    ) where
        R: RangeBounds<Pubkey> + std::fmt::Debug + Sync,
    {
        self.accounts_db
            .accounts_index
            .hold_range_in_memory(range, start_holding, thread_pool)
    }

    pub fn load_to_collect_rent_eagerly<R: RangeBounds<Pubkey> + std::fmt::Debug>(
        &self,
        ancestors: &Ancestors,
        range: R,
    ) -> Vec<PubkeyAccountSlot> {
        let mut collector = Vec::new();
        self.accounts_db.range_scan_accounts(
            "", // disable logging of this. We now parallelize it and this results in multiple parallel logs
            ancestors,
            range,
            &ScanConfig::new(true),
            |option| Self::load_with_slot(&mut collector, option),
        );
        collector
    }

    /// Slow because lock is held for 1 operation instead of many.
    /// WARNING: This noncached version is only to be used for tests/benchmarking
    /// as bypassing the cache in general is not supported
    pub fn store_slow_uncached(&self, slot: Slot, pubkey: &Pubkey, account: &AccountSharedData) {
        self.accounts_db.store_uncached(slot, &[(pubkey, account)]);
    }

    fn lock_account(
        &self,
        account_locks: &mut AccountLocks,
        writable_keys: Vec<&Pubkey>,
        readonly_keys: Vec<&Pubkey>,
    ) -> Result<()> {
        for k in writable_keys.iter() {
            if account_locks.is_locked_write(k) || account_locks.is_locked_readonly(k) {
                debug!("Writable account in use: {:?}", k);
                return Err(TransactionError::AccountInUse);
            }
        }
        for k in readonly_keys.iter() {
            if account_locks.is_locked_write(k) {
                debug!("Read-only account in use: {:?}", k);
                return Err(TransactionError::AccountInUse);
            }
        }

        for k in writable_keys {
            account_locks.write_locks.insert(*k);
        }

        for k in readonly_keys {
            if !account_locks.lock_readonly(k) {
                account_locks.insert_new_readonly(k);
            }
        }

        Ok(())
    }

    fn unlock_account(
        &self,
        account_locks: &mut AccountLocks,
        writable_keys: Vec<&Pubkey>,
        readonly_keys: Vec<&Pubkey>,
    ) {
        for k in writable_keys {
            account_locks.unlock_write(k);
        }
        for k in readonly_keys {
            account_locks.unlock_readonly(k);
        }
    }

    /// This function will prevent multiple threads from modifying the same account state at the
    /// same time
    #[must_use]
    #[allow(clippy::needless_collect)]
    pub fn lock_accounts<'a>(
        &self,
        txs: impl Iterator<Item = &'a SanitizedTransaction>,
        tx_account_lock_limit: usize,
    ) -> Vec<Result<()>> {
        let tx_account_locks_results: Vec<Result<_>> = txs
            .map(|tx| tx.get_account_locks(tx_account_lock_limit))
            .collect();
        self.lock_accounts_inner(tx_account_locks_results)
    }

    #[must_use]
    #[allow(clippy::needless_collect)]
    pub fn lock_accounts_with_results<'a>(
        &self,
        txs: impl Iterator<Item = &'a SanitizedTransaction>,
        results: impl Iterator<Item = Result<()>>,
        tx_account_lock_limit: usize,
    ) -> Vec<Result<()>> {
        let tx_account_locks_results: Vec<Result<_>> = txs
            .zip(results)
            .map(|(tx, result)| match result {
                Ok(()) => tx.get_account_locks(tx_account_lock_limit),
                Err(err) => Err(err),
            })
            .collect();
        self.lock_accounts_inner(tx_account_locks_results)
    }

    #[must_use]
    fn lock_accounts_inner(
        &self,
        tx_account_locks_results: Vec<Result<TransactionAccountLocks>>,
    ) -> Vec<Result<()>> {
        let account_locks = &mut self.account_locks.lock().unwrap();
        tx_account_locks_results
            .into_iter()
            .map(|tx_account_locks_result| match tx_account_locks_result {
                Ok(tx_account_locks) => self.lock_account(
                    account_locks,
                    tx_account_locks.writable,
                    tx_account_locks.readonly,
                ),
                Err(err) => Err(err),
            })
            .collect()
    }

    /// Once accounts are unlocked, new transactions that modify that state can enter the pipeline
    #[allow(clippy::needless_collect)]
    pub fn unlock_accounts<'a>(
        &self,
        txs: impl Iterator<Item = &'a SanitizedTransaction>,
        results: &[Result<()>],
    ) {
        let keys: Vec<_> = txs
            .zip(results)
            .filter_map(|(tx, res)| match res {
                Err(TransactionError::AccountLoadedTwice)
                | Err(TransactionError::AccountInUse)
                | Err(TransactionError::SanitizeFailure)
                | Err(TransactionError::TooManyAccountLocks)
                | Err(TransactionError::WouldExceedMaxBlockCostLimit)
                | Err(TransactionError::WouldExceedMaxVoteCostLimit)
                | Err(TransactionError::WouldExceedMaxAccountCostLimit)
                | Err(TransactionError::WouldExceedAccountDataBlockLimit)
                | Err(TransactionError::WouldExceedAccountDataTotalLimit) => None,
                _ => Some(tx.get_account_locks_unchecked()),
            })
            .collect();
        let mut account_locks = self.account_locks.lock().unwrap();
        debug!("bank unlock accounts");
        keys.into_iter().for_each(|keys| {
            self.unlock_account(&mut account_locks, keys.writable, keys.readonly);
        });
    }

    /// Store the accounts into the DB
    // allow(clippy) needed for various gating flags
    #[allow(clippy::too_many_arguments)]
    pub fn store_cached(
        &self,
        slot: Slot,
        txs: &[SanitizedTransaction],
        res: &[TransactionExecutionResult],
        loaded: &mut [TransactionLoadResult],
        rent_collector: &RentCollector,
        durable_nonce: &DurableNonce,
        lamports_per_signature: u64,
        include_slot_in_hash: IncludeSlotInHash,
    ) {
        let (accounts_to_store, transactions) = self.collect_accounts_to_store(
            txs,
            res,
            loaded,
            rent_collector,
            durable_nonce,
            lamports_per_signature,
        );
        self.accounts_db.store_cached_inline_update_index(
            (slot, &accounts_to_store[..], include_slot_in_hash),
            Some(&transactions),
        );
    }

    pub fn store_accounts_cached<'a, T: ReadableAccount + Sync + ZeroLamport + 'a>(
        &self,
        accounts: impl StorableAccounts<'a, T>,
    ) {
        self.accounts_db.store_cached(accounts, None)
    }

    /// Add a slot to root.  Root slots cannot be purged
    pub fn add_root(&self, slot: Slot) -> AccountsAddRootTiming {
        self.accounts_db.add_root(slot)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_accounts_to_store<'a>(
        &self,
        txs: &'a [SanitizedTransaction],
        execution_results: &'a [TransactionExecutionResult],
        load_results: &'a mut [TransactionLoadResult],
        _rent_collector: &RentCollector,
        durable_nonce: &DurableNonce,
        lamports_per_signature: u64,
    ) -> (
        Vec<(&'a Pubkey, &'a AccountSharedData)>,
        Vec<Option<&'a SanitizedTransaction>>,
    ) {
        let mut accounts = Vec::with_capacity(load_results.len());
        let mut transactions = Vec::with_capacity(load_results.len());
        for (i, ((tx_load_result, nonce), tx)) in load_results.iter_mut().zip(txs).enumerate() {
            if tx_load_result.is_err() {
                // Don't store any accounts if tx failed to load
                continue;
            }

            let execution_status = match &execution_results[i] {
                TransactionExecutionResult::Executed { details, .. } => &details.status,
                // Don't store any accounts if tx wasn't executed
                TransactionExecutionResult::NotExecuted(_) => continue,
            };

            let maybe_nonce = match (execution_status, &*nonce) {
                (Ok(_), _) => None, // Success, don't do any additional nonce processing
                (Err(_), Some(nonce)) => {
                    Some((nonce, true /* rollback */))
                }
                (Err(_), None) => {
                    // Fees for failed transactions which don't use durable nonces are
                    // deducted in Bank::filter_program_errors_and_collect_fee
                    continue;
                }
            };

            let message = tx.message();
            let loaded_transaction = tx_load_result.as_mut().unwrap();
            let mut fee_payer_index = None;
            for (i, (address, account)) in (0..message.account_keys().len())
                .zip(loaded_transaction.accounts.iter_mut())
                .filter(|(i, _)| message.is_non_loader_key(*i))
            {
                if fee_payer_index.is_none() {
                    fee_payer_index = Some(i);
                }
                let is_fee_payer = Some(i) == fee_payer_index;
                if message.is_writable(i) {
                    let is_nonce_account = prepare_if_nonce_account(
                        address,
                        account,
                        execution_status,
                        is_fee_payer,
                        maybe_nonce,
                        durable_nonce,
                        lamports_per_signature,
                    );

                    if execution_status.is_ok() || is_nonce_account || is_fee_payer {
                        // Add to the accounts to store
                        accounts.push((&*address, &*account));
                        transactions.push(Some(tx));
                    }
                }
            }
        }
        (accounts, transactions)
    }
}

fn prepare_if_nonce_account(
    address: &Pubkey,
    account: &mut AccountSharedData,
    execution_result: &Result<()>,
    is_fee_payer: bool,
    maybe_nonce: Option<(&NonceFull, bool)>,
    &durable_nonce: &DurableNonce,
    lamports_per_signature: u64,
) -> bool {
    if let Some((nonce, rollback)) = maybe_nonce {
        if address == nonce.address() {
            if rollback {
                // The transaction failed which would normally drop the account
                // processing changes, since this account is now being included
                // in the accounts written back to the db, roll it back to
                // pre-processing state.
                *account = nonce.account().clone();
            }

            // Advance the stored blockhash to prevent fee theft by someone
            // replaying nonce transactions that have failed with an
            // `InstructionError`.
            //
            // Since we know we are dealing with a valid nonce account,
            // unwrap is safe here
            let nonce_versions = StateMut::<NonceVersions>::state(nonce.account()).unwrap();
            if let NonceState::Initialized(ref data) = nonce_versions.state() {
                let nonce_state = NonceState::new_initialized(
                    &data.authority,
                    durable_nonce,
                    lamports_per_signature,
                );
                let nonce_versions = NonceVersions::new(nonce_state);
                account.set_state(&nonce_versions).unwrap();
            }
            true
        } else {
            if execution_result.is_err() && is_fee_payer {
                if let Some(fee_payer_account) = nonce.fee_payer_account() {
                    // Instruction error and fee-payer for this nonce tx is not
                    // the nonce account itself, rollback the fee payer to the
                    // fee-paid original state.
                    *account = fee_payer_account.clone();
                }
            }

            false
        }
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            rent_collector::RentCollector,
            transaction_results::{DurableNonceFee, TransactionExecutionDetails},
        },
        assert_matches::assert_matches,
        solana_program_runtime::prioritization_fee::{
            PrioritizationFeeDetails, PrioritizationFeeType,
        },
        solana_sdk::{
            account::{AccountSharedData, WritableAccount},
            address_lookup_table::state::LookupTableMeta,
            compute_budget::ComputeBudgetInstruction,
            epoch_schedule::EpochSchedule,
            genesis_config::ClusterType,
            hash::Hash,
            instruction::{CompiledInstruction, InstructionError},
            message::{Message, MessageHeader},
            nonce, nonce_account,
            rent::Rent,
            signature::{keypair_from_seed, signers::Signers, Keypair, Signer},
            system_instruction, system_program,
            transaction::{Transaction, MAX_TX_ACCOUNT_LOCKS},
        },
        std::{
            borrow::Cow,
            convert::TryFrom,
            sync::atomic::{AtomicBool, AtomicU64, Ordering},
            thread, time,
        },
    };

    fn new_sanitized_tx<T: Signers>(
        from_keypairs: &T,
        message: Message,
        recent_blockhash: Hash,
    ) -> SanitizedTransaction {
        SanitizedTransaction::from_transaction_for_tests(Transaction::new(
            from_keypairs,
            message,
            recent_blockhash,
        ))
    }

    fn new_execution_result(
        status: Result<()>,
        nonce: Option<&NonceFull>,
    ) -> TransactionExecutionResult {
        TransactionExecutionResult::Executed {
            details: TransactionExecutionDetails {
                status,
                log_messages: None,
                inner_instructions: None,
                durable_nonce_fee: nonce.map(DurableNonceFee::from),
                return_data: None,
                executed_units: 0,
                accounts_data_len_delta: 0,
            },
            programs_modified_by_tx: Box::<LoadedProgramsForTxBatch>::default(),
            programs_updated_only_for_global_cache: Box::<LoadedProgramsForTxBatch>::default(),
        }
    }

    fn load_accounts_with_fee_and_rent(
        tx: Transaction,
        ka: &[TransactionAccount],
        lamports_per_signature: u64,
        rent_collector: &RentCollector,
        error_counters: &mut TransactionErrorMetrics,
        feature_set: &FeatureSet,
        fee_structure: &FeeStructure,
    ) -> Vec<TransactionLoadResult> {
        let mut hash_queue = BlockhashQueue::new(100);
        hash_queue.register_hash(&tx.message().recent_blockhash, lamports_per_signature);
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        for ka in ka.iter() {
            accounts.store_for_tests(0, &ka.0, &ka.1);
        }

        let ancestors = vec![(0, 0)].into_iter().collect();
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);
        accounts.load_accounts(
            &ancestors,
            &[sanitized_tx],
            vec![(Ok(()), None)],
            &hash_queue,
            error_counters,
            rent_collector,
            feature_set,
            fee_structure,
            None,
            RewardInterval::OutsideInterval,
            &HashMap::new(),
            &LoadedProgramsForTxBatch::default(),
        )
    }

    /// get a feature set with all features activated
    /// with the optional except of 'exclude'
    fn all_features_except(exclude: Option<&[Pubkey]>) -> FeatureSet {
        let mut features = FeatureSet::all_enabled();
        if let Some(exclude) = exclude {
            features.active.retain(|k, _v| !exclude.contains(k));
        }
        features
    }

    fn load_accounts_with_fee(
        tx: Transaction,
        ka: &[TransactionAccount],
        lamports_per_signature: u64,
        error_counters: &mut TransactionErrorMetrics,
        exclude_features: Option<&[Pubkey]>,
    ) -> Vec<TransactionLoadResult> {
        load_accounts_with_fee_and_rent(
            tx,
            ka,
            lamports_per_signature,
            &RentCollector::default(),
            error_counters,
            &all_features_except(exclude_features),
            &FeeStructure::default(),
        )
    }

    fn load_accounts(
        tx: Transaction,
        ka: &[TransactionAccount],
        error_counters: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionLoadResult> {
        load_accounts_with_fee(tx, ka, 0, error_counters, None)
    }

    fn load_accounts_with_excluded_features(
        tx: Transaction,
        ka: &[TransactionAccount],
        error_counters: &mut TransactionErrorMetrics,
        exclude_features: Option<&[Pubkey]>,
    ) -> Vec<TransactionLoadResult> {
        load_accounts_with_fee(tx, ka, 0, error_counters, exclude_features)
    }

    #[test]
    fn test_hold_range_in_memory() {
        let accts = Accounts::default_for_tests();
        let range = Pubkey::from([0; 32])..=Pubkey::from([0xff; 32]);
        accts.hold_range_in_memory(&range, true, &test_thread_pool());
        accts.hold_range_in_memory(&range, false, &test_thread_pool());
        accts.hold_range_in_memory(&range, true, &test_thread_pool());
        accts.hold_range_in_memory(&range, true, &test_thread_pool());
        accts.hold_range_in_memory(&range, false, &test_thread_pool());
        accts.hold_range_in_memory(&range, false, &test_thread_pool());
    }

    #[test]
    fn test_hold_range_in_memory2() {
        let accts = Accounts::default_for_tests();
        let range = Pubkey::from([0; 32])..=Pubkey::from([0xff; 32]);
        let idx = &accts.accounts_db.accounts_index;
        let bins = idx.account_maps.len();
        // use bins * 2 to get the first half of the range within bin 0
        let bins_2 = bins * 2;
        let binner = crate::pubkey_bins::PubkeyBinCalculator24::new(bins_2);
        let range2 =
            binner.lowest_pubkey_from_bin(0, bins_2)..binner.lowest_pubkey_from_bin(1, bins_2);
        let range2_inclusive = range2.start..=range2.end;
        assert_eq!(0, idx.bin_calculator.bin_from_pubkey(&range2.start));
        assert_eq!(0, idx.bin_calculator.bin_from_pubkey(&range2.end));
        accts.hold_range_in_memory(&range, true, &test_thread_pool());
        idx.account_maps.iter().for_each(|map| {
            assert_eq!(
                map.cache_ranges_held.read().unwrap().to_vec(),
                vec![range.clone()]
            );
        });
        accts.hold_range_in_memory(&range2, true, &test_thread_pool());
        idx.account_maps.iter().enumerate().for_each(|(bin, map)| {
            let expected = if bin == 0 {
                vec![range.clone(), range2_inclusive.clone()]
            } else {
                vec![range.clone()]
            };
            assert_eq!(
                map.cache_ranges_held.read().unwrap().to_vec(),
                expected,
                "bin: {bin}"
            );
        });
        accts.hold_range_in_memory(&range, false, &test_thread_pool());
        accts.hold_range_in_memory(&range2, false, &test_thread_pool());
    }

    fn test_thread_pool() -> rayon::ThreadPool {
        crate::accounts_db::make_min_priority_thread_pool()
    }

    #[test]
    fn test_load_accounts_no_account_0_exists() {
        let accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts = load_accounts(tx, &accounts, &mut error_counters);

        assert_eq!(error_counters.account_not_found, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::AccountNotFound), None,),
        );
    }

    #[test]
    fn test_load_accounts_unknown_program_id() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let account = AccountSharedData::new(2, 1, &Pubkey::default());
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![Pubkey::default()],
            instructions,
        );

        let loaded_accounts = load_accounts(tx, &accounts, &mut error_counters);

        assert_eq!(error_counters.account_not_found, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::ProgramAccountNotFound), None,)
        );
    }

    #[test]
    fn test_load_accounts_insufficient_funds() {
        let lamports_per_signature = 5000;
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&remove_deprecated_request_unit_ix::id());

        let message = SanitizedMessage::try_from(tx.message().clone()).unwrap();
        let fee = FeeStructure::default().calculate_fee(
            &message,
            lamports_per_signature,
            &ComputeBudget::fee_budget_limits(message.program_instructions_iter(), &feature_set),
            true,
            false,
        );
        assert_eq!(fee, lamports_per_signature);

        let loaded_accounts = load_accounts_with_fee(
            tx,
            &accounts,
            lamports_per_signature,
            &mut error_counters,
            None,
        );

        assert_eq!(error_counters.insufficient_funds, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0].clone(),
            (Err(TransactionError::InsufficientFundsForFee), None,),
        );
    }

    #[test]
    fn test_load_accounts_invalid_account_for_fee() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();

        let account = AccountSharedData::new(1, 1, &solana_sdk::pubkey::new_rand()); // <-- owner is not the system program
        accounts.push((key0, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts = load_accounts(tx, &accounts, &mut error_counters);

        assert_eq!(error_counters.invalid_account_for_fee, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::InvalidAccountForFee), None,),
        );
    }

    #[test]
    fn test_load_accounts_fee_payer_is_nonce() {
        let lamports_per_signature = 5000;
        let mut error_counters = TransactionErrorMetrics::default();
        let rent_collector = RentCollector::new(
            0,
            EpochSchedule::default(),
            500_000.0,
            Rent {
                lamports_per_byte_year: 42,
                ..Rent::default()
            },
        );
        let min_balance = rent_collector.rent.minimum_balance(NonceState::size());
        let nonce = Keypair::new();
        let mut accounts = vec![(
            nonce.pubkey(),
            AccountSharedData::new_data(
                min_balance + lamports_per_signature,
                &NonceVersions::new(NonceState::Initialized(nonce::state::Data::default())),
                &system_program::id(),
            )
            .unwrap(),
        )];
        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&nonce],
            &[],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        // Fee leaves min_balance balance succeeds
        let loaded_accounts = load_accounts_with_fee_and_rent(
            tx.clone(),
            &accounts,
            lamports_per_signature,
            &rent_collector,
            &mut error_counters,
            &all_features_except(None),
            &FeeStructure::default(),
        );
        assert_eq!(loaded_accounts.len(), 1);
        let (load_res, _nonce) = &loaded_accounts[0];
        let loaded_transaction = load_res.as_ref().unwrap();
        assert_eq!(loaded_transaction.accounts[0].1.lamports(), min_balance);

        // Fee leaves zero balance fails
        accounts[0].1.set_lamports(lamports_per_signature);
        let loaded_accounts = load_accounts_with_fee_and_rent(
            tx.clone(),
            &accounts,
            lamports_per_signature,
            &rent_collector,
            &mut error_counters,
            &FeatureSet::all_enabled(),
            &FeeStructure::default(),
        );
        assert_eq!(loaded_accounts.len(), 1);
        let (load_res, _nonce) = &loaded_accounts[0];
        assert_eq!(*load_res, Err(TransactionError::InsufficientFundsForFee));

        // Fee leaves non-zero, but sub-min_balance balance fails
        accounts[0]
            .1
            .set_lamports(lamports_per_signature + min_balance / 2);
        let loaded_accounts = load_accounts_with_fee_and_rent(
            tx,
            &accounts,
            lamports_per_signature,
            &rent_collector,
            &mut error_counters,
            &FeatureSet::all_enabled(),
            &FeeStructure::default(),
        );
        assert_eq!(loaded_accounts.len(), 1);
        let (load_res, _nonce) = &loaded_accounts[0];
        assert_eq!(*load_res, Err(TransactionError::InsufficientFundsForFee));
    }

    #[test]
    fn test_load_accounts_no_loaders() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(2, 1, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[key1],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts =
            load_accounts_with_excluded_features(tx, &accounts, &mut error_counters, None);

        assert_eq!(error_counters.account_not_found, 0);
        assert_eq!(loaded_accounts.len(), 1);
        match &loaded_accounts[0] {
            (Ok(loaded_transaction), _nonce) => {
                assert_eq!(loaded_transaction.accounts.len(), 3);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.program_indices.len(), 1);
                assert_eq!(loaded_transaction.program_indices[0].len(), 0);
            }
            (Err(e), _nonce) => panic!("{e}"),
        }
    }

    #[test]
    fn test_load_accounts_bad_owner() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &Pubkey::default());
        account.set_executable(true);
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1],
            instructions,
        );

        let loaded_accounts = load_accounts(tx, &accounts, &mut error_counters);

        assert_eq!(error_counters.account_not_found, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::ProgramAccountNotFound), None,)
        );
    }

    #[test]
    fn test_load_accounts_not_executable() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let account = AccountSharedData::new(40, 1, &native_loader::id());
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1],
            instructions,
        );

        let loaded_accounts = load_accounts(tx, &accounts, &mut error_counters);

        assert_eq!(error_counters.invalid_program_for_execution, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::InvalidProgramForExecution), None,)
        );
    }

    #[test]
    fn test_filter_executable_program_accounts() {
        let mut tx_accounts: Vec<TransactionAccount> = Vec::new();

        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let non_program_pubkey1 = Pubkey::new_unique();
        let non_program_pubkey2 = Pubkey::new_unique();
        let program1_pubkey = Pubkey::new_unique();
        let program2_pubkey = Pubkey::new_unique();
        let account1_pubkey = Pubkey::new_unique();
        let account2_pubkey = Pubkey::new_unique();
        let account3_pubkey = Pubkey::new_unique();
        let account4_pubkey = Pubkey::new_unique();

        let account5_pubkey = Pubkey::new_unique();

        tx_accounts.push((
            non_program_pubkey1,
            AccountSharedData::new(1, 10, &account5_pubkey),
        ));
        tx_accounts.push((
            non_program_pubkey2,
            AccountSharedData::new(1, 10, &account5_pubkey),
        ));
        tx_accounts.push((
            program1_pubkey,
            AccountSharedData::new(40, 1, &account5_pubkey),
        ));
        tx_accounts.push((
            program2_pubkey,
            AccountSharedData::new(40, 1, &account5_pubkey),
        ));
        tx_accounts.push((
            account1_pubkey,
            AccountSharedData::new(1, 10, &non_program_pubkey1),
        ));
        tx_accounts.push((
            account2_pubkey,
            AccountSharedData::new(1, 10, &non_program_pubkey2),
        ));
        tx_accounts.push((
            account3_pubkey,
            AccountSharedData::new(40, 1, &program1_pubkey),
        ));
        tx_accounts.push((
            account4_pubkey,
            AccountSharedData::new(40, 1, &program2_pubkey),
        ));

        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        for tx_account in tx_accounts.iter() {
            accounts.store_for_tests(0, &tx_account.0, &tx_account.1);
        }

        let mut hash_queue = BlockhashQueue::new(100);

        let tx1 = Transaction::new_with_compiled_instructions(
            &[&keypair1],
            &[non_program_pubkey1],
            Hash::new_unique(),
            vec![account1_pubkey, account2_pubkey, account3_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        hash_queue.register_hash(&tx1.message().recent_blockhash, 0);
        let sanitized_tx1 = SanitizedTransaction::from_transaction_for_tests(tx1);

        let tx2 = Transaction::new_with_compiled_instructions(
            &[&keypair2],
            &[non_program_pubkey2],
            Hash::new_unique(),
            vec![account4_pubkey, account3_pubkey, account2_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        hash_queue.register_hash(&tx2.message().recent_blockhash, 0);
        let sanitized_tx2 = SanitizedTransaction::from_transaction_for_tests(tx2);

        let ancestors = vec![(0, 0)].into_iter().collect();
        let owners = &[program1_pubkey, program2_pubkey];
        let programs = accounts.filter_executable_program_accounts(
            &ancestors,
            &[sanitized_tx1, sanitized_tx2],
            &mut [(Ok(()), None), (Ok(()), None)],
            owners,
            &hash_queue,
        );

        // The result should contain only account3_pubkey, and account4_pubkey as the program accounts
        assert_eq!(programs.len(), 2);
        assert_eq!(
            programs
                .get(&account3_pubkey)
                .expect("failed to find the program account"),
            &(&program1_pubkey, 2)
        );
        assert_eq!(
            programs
                .get(&account4_pubkey)
                .expect("failed to find the program account"),
            &(&program2_pubkey, 1)
        );
    }

    #[test]
    fn test_filter_executable_program_accounts_invalid_blockhash() {
        let mut tx_accounts: Vec<TransactionAccount> = Vec::new();

        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let non_program_pubkey1 = Pubkey::new_unique();
        let non_program_pubkey2 = Pubkey::new_unique();
        let program1_pubkey = Pubkey::new_unique();
        let program2_pubkey = Pubkey::new_unique();
        let account1_pubkey = Pubkey::new_unique();
        let account2_pubkey = Pubkey::new_unique();
        let account3_pubkey = Pubkey::new_unique();
        let account4_pubkey = Pubkey::new_unique();

        let account5_pubkey = Pubkey::new_unique();

        tx_accounts.push((
            non_program_pubkey1,
            AccountSharedData::new(1, 10, &account5_pubkey),
        ));
        tx_accounts.push((
            non_program_pubkey2,
            AccountSharedData::new(1, 10, &account5_pubkey),
        ));
        tx_accounts.push((
            program1_pubkey,
            AccountSharedData::new(40, 1, &account5_pubkey),
        ));
        tx_accounts.push((
            program2_pubkey,
            AccountSharedData::new(40, 1, &account5_pubkey),
        ));
        tx_accounts.push((
            account1_pubkey,
            AccountSharedData::new(1, 10, &non_program_pubkey1),
        ));
        tx_accounts.push((
            account2_pubkey,
            AccountSharedData::new(1, 10, &non_program_pubkey2),
        ));
        tx_accounts.push((
            account3_pubkey,
            AccountSharedData::new(40, 1, &program1_pubkey),
        ));
        tx_accounts.push((
            account4_pubkey,
            AccountSharedData::new(40, 1, &program2_pubkey),
        ));

        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        for tx_account in tx_accounts.iter() {
            accounts.store_for_tests(0, &tx_account.0, &tx_account.1);
        }

        let mut hash_queue = BlockhashQueue::new(100);

        let tx1 = Transaction::new_with_compiled_instructions(
            &[&keypair1],
            &[non_program_pubkey1],
            Hash::new_unique(),
            vec![account1_pubkey, account2_pubkey, account3_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        hash_queue.register_hash(&tx1.message().recent_blockhash, 0);
        let sanitized_tx1 = SanitizedTransaction::from_transaction_for_tests(tx1);

        let tx2 = Transaction::new_with_compiled_instructions(
            &[&keypair2],
            &[non_program_pubkey2],
            Hash::new_unique(),
            vec![account4_pubkey, account3_pubkey, account2_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        // Let's not register blockhash from tx2. This should cause the tx2 to fail
        let sanitized_tx2 = SanitizedTransaction::from_transaction_for_tests(tx2);

        let ancestors = vec![(0, 0)].into_iter().collect();
        let owners = &[program1_pubkey, program2_pubkey];
        let mut lock_results = vec![(Ok(()), None), (Ok(()), None)];
        let programs = accounts.filter_executable_program_accounts(
            &ancestors,
            &[sanitized_tx1, sanitized_tx2],
            &mut lock_results,
            owners,
            &hash_queue,
        );

        // The result should contain only account3_pubkey as the program accounts
        assert_eq!(programs.len(), 1);
        assert_eq!(
            programs
                .get(&account3_pubkey)
                .expect("failed to find the program account"),
            &(&program1_pubkey, 1)
        );
        assert_eq!(lock_results[1].0, Err(TransactionError::BlockhashNotFound));
    }

    #[test]
    fn test_load_accounts_multiple_loaders() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);
        let key2 = Pubkey::from([6u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &Pubkey::default());
        account.set_executable(true);
        account.set_rent_epoch(1);
        account.set_owner(native_loader::id());
        accounts.push((key1, account));

        let mut account = AccountSharedData::new(41, 1, &Pubkey::default());
        account.set_executable(true);
        account.set_rent_epoch(1);
        account.set_owner(key1);
        accounts.push((key2, account));

        let instructions = vec![
            CompiledInstruction::new(1, &(), vec![0]),
            CompiledInstruction::new(2, &(), vec![0]),
        ];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1, key2],
            instructions,
        );

        let loaded_accounts =
            load_accounts_with_excluded_features(tx, &accounts, &mut error_counters, None);

        assert_eq!(error_counters.account_not_found, 0);
        assert_eq!(loaded_accounts.len(), 1);
        match &loaded_accounts[0] {
            (Ok(loaded_transaction), _nonce) => {
                assert_eq!(loaded_transaction.accounts.len(), 4);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.program_indices.len(), 2);
                assert_eq!(loaded_transaction.program_indices[0].len(), 1);
                assert_eq!(loaded_transaction.program_indices[1].len(), 2);
                for program_indices in loaded_transaction.program_indices.iter() {
                    for (i, program_index) in program_indices.iter().enumerate() {
                        // +1 to skip first not loader account
                        assert_eq!(
                            loaded_transaction.accounts[*program_index as usize].0,
                            accounts[i + 1].0
                        );
                        assert_eq!(
                            loaded_transaction.accounts[*program_index as usize].1,
                            accounts[i + 1].1
                        );
                    }
                }
            }
            (Err(e), _nonce) => panic!("{e}"),
        }
    }

    #[test]
    fn test_load_lookup_table_addresses_account_not_found() {
        let ancestors = vec![(0, 0)].into_iter().collect();
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        let invalid_table_key = Pubkey::new_unique();
        let address_table_lookup = MessageAddressTableLookup {
            account_key: invalid_table_key,
            writable_indexes: vec![],
            readonly_indexes: vec![],
        };

        assert_eq!(
            accounts.load_lookup_table_addresses(
                &ancestors,
                &address_table_lookup,
                &SlotHashes::default(),
            ),
            Err(AddressLookupError::LookupTableAccountNotFound),
        );
    }

    #[test]
    fn test_load_lookup_table_addresses_invalid_account_owner() {
        let ancestors = vec![(0, 0)].into_iter().collect();
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        let invalid_table_key = Pubkey::new_unique();
        let mut invalid_table_account = AccountSharedData::default();
        invalid_table_account.set_lamports(1);
        accounts.store_slow_uncached(0, &invalid_table_key, &invalid_table_account);

        let address_table_lookup = MessageAddressTableLookup {
            account_key: invalid_table_key,
            writable_indexes: vec![],
            readonly_indexes: vec![],
        };

        assert_eq!(
            accounts.load_lookup_table_addresses(
                &ancestors,
                &address_table_lookup,
                &SlotHashes::default(),
            ),
            Err(AddressLookupError::InvalidAccountOwner),
        );
    }

    #[test]
    fn test_load_lookup_table_addresses_invalid_account_data() {
        let ancestors = vec![(0, 0)].into_iter().collect();
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        let invalid_table_key = Pubkey::new_unique();
        let invalid_table_account =
            AccountSharedData::new(1, 0, &address_lookup_table::program::id());
        accounts.store_slow_uncached(0, &invalid_table_key, &invalid_table_account);

        let address_table_lookup = MessageAddressTableLookup {
            account_key: invalid_table_key,
            writable_indexes: vec![],
            readonly_indexes: vec![],
        };

        assert_eq!(
            accounts.load_lookup_table_addresses(
                &ancestors,
                &address_table_lookup,
                &SlotHashes::default(),
            ),
            Err(AddressLookupError::InvalidAccountData),
        );
    }

    #[test]
    fn test_load_lookup_table_addresses() {
        let ancestors = vec![(1, 1), (0, 0)].into_iter().collect();
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        let table_key = Pubkey::new_unique();
        let table_addresses = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let table_account = {
            let table_state = AddressLookupTable {
                meta: LookupTableMeta::default(),
                addresses: Cow::Owned(table_addresses.clone()),
            };
            AccountSharedData::create(
                1,
                table_state.serialize_for_tests().unwrap(),
                address_lookup_table::program::id(),
                false,
                0,
            )
        };
        accounts.store_slow_uncached(0, &table_key, &table_account);

        let address_table_lookup = MessageAddressTableLookup {
            account_key: table_key,
            writable_indexes: vec![0],
            readonly_indexes: vec![1],
        };

        assert_eq!(
            accounts.load_lookup_table_addresses(
                &ancestors,
                &address_table_lookup,
                &SlotHashes::default(),
            ),
            Ok(LoadedAddresses {
                writable: vec![table_addresses[0]],
                readonly: vec![table_addresses[1]],
            }),
        );
    }

    #[test]
    fn test_load_by_program_slot() {
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        // Load accounts owned by various programs into AccountsDb
        let pubkey0 = solana_sdk::pubkey::new_rand();
        let account0 = AccountSharedData::new(1, 0, &Pubkey::from([2; 32]));
        accounts.store_slow_uncached(0, &pubkey0, &account0);
        let pubkey1 = solana_sdk::pubkey::new_rand();
        let account1 = AccountSharedData::new(1, 0, &Pubkey::from([2; 32]));
        accounts.store_slow_uncached(0, &pubkey1, &account1);
        let pubkey2 = solana_sdk::pubkey::new_rand();
        let account2 = AccountSharedData::new(1, 0, &Pubkey::from([3; 32]));
        accounts.store_slow_uncached(0, &pubkey2, &account2);

        let loaded = accounts.load_by_program_slot(0, Some(&Pubkey::from([2; 32])));
        assert_eq!(loaded.len(), 2);
        let loaded = accounts.load_by_program_slot(0, Some(&Pubkey::from([3; 32])));
        assert_eq!(loaded, vec![(pubkey2, account2)]);
        let loaded = accounts.load_by_program_slot(0, Some(&Pubkey::from([4; 32])));
        assert_eq!(loaded, vec![]);
    }

    #[test]
    fn test_load_accounts_executable_with_write_lock() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);
        let key2 = Pubkey::from([6u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &native_loader::id());
        account.set_executable(true);
        account.set_rent_epoch(1);
        accounts.push((key1, account));

        let mut account = AccountSharedData::new(40, 1, &native_loader::id());
        account.set_executable(true);
        account.set_rent_epoch(1);
        accounts.push((key2, account));

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let mut message = Message::new_with_compiled_instructions(
            1,
            0,
            1, // only one executable marked as readonly
            vec![key0, key1, key2],
            Hash::default(),
            instructions,
        );
        let tx = Transaction::new(&[&keypair], message.clone(), Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx,
            &accounts,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::InvalidWritableAccount), None)
        );

        // Mark executables as readonly
        message.account_keys = vec![key0, key1, key2]; // revert key change
        message.header.num_readonly_unsigned_accounts = 2; // mark both executables as readonly
        let tx = Transaction::new(&[&keypair], message, Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx,
            &accounts,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        let result = loaded_accounts[0].0.as_ref().unwrap();
        assert_eq!(result.accounts[..2], accounts[..2]);
        assert_eq!(
            result.accounts[result.program_indices[0][0] as usize],
            accounts[2]
        );
    }

    #[test]
    fn test_load_accounts_upgradeable_with_write_lock() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);
        let key2 = Pubkey::from([6u8; 32]);
        let programdata_key1 = Pubkey::from([7u8; 32]);
        let programdata_key2 = Pubkey::from([8u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let program_data = UpgradeableLoaderState::ProgramData {
            slot: 42,
            upgrade_authority_address: None,
        };

        let program = UpgradeableLoaderState::Program {
            programdata_address: programdata_key1,
        };
        let mut account =
            AccountSharedData::new_data(40, &program, &bpf_loader_upgradeable::id()).unwrap();
        account.set_executable(true);
        account.set_rent_epoch(1);
        accounts.push((key1, account));
        let mut account =
            AccountSharedData::new_data(40, &program_data, &bpf_loader_upgradeable::id()).unwrap();
        account.set_rent_epoch(1);
        accounts.push((programdata_key1, account));

        let program = UpgradeableLoaderState::Program {
            programdata_address: programdata_key2,
        };
        let mut account =
            AccountSharedData::new_data(40, &program, &bpf_loader_upgradeable::id()).unwrap();
        account.set_executable(true);
        account.set_rent_epoch(1);
        accounts.push((key2, account));
        let mut account =
            AccountSharedData::new_data(40, &program_data, &bpf_loader_upgradeable::id()).unwrap();
        account.set_rent_epoch(1);
        accounts.push((programdata_key2, account));

        let mut account = AccountSharedData::new(40, 1, &native_loader::id()); // create mock bpf_loader_upgradeable
        account.set_executable(true);
        account.set_rent_epoch(1);
        accounts.push((bpf_loader_upgradeable::id(), account));

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let mut message = Message::new_with_compiled_instructions(
            1,
            0,
            1, // only one executable marked as readonly
            vec![key0, key1, key2],
            Hash::default(),
            instructions,
        );
        let tx = Transaction::new(&[&keypair], message.clone(), Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx.clone(),
            &accounts,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::InvalidWritableAccount), None)
        );

        // Solution 0: Include feature simplify_writable_program_account_check
        let loaded_accounts =
            load_accounts_with_excluded_features(tx, &accounts, &mut error_counters, None);

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);

        // Solution 1: include bpf_loader_upgradeable account
        message.account_keys = vec![key0, key1, bpf_loader_upgradeable::id()];
        let tx = Transaction::new(&[&keypair], message.clone(), Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx,
            &accounts,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        let result = loaded_accounts[0].0.as_ref().unwrap();
        assert_eq!(result.accounts[..2], accounts[..2]);
        assert_eq!(
            result.accounts[result.program_indices[0][0] as usize],
            accounts[5]
        );

        // Solution 2: mark programdata as readonly
        message.account_keys = vec![key0, key1, key2]; // revert key change
        message.header.num_readonly_unsigned_accounts = 2; // mark both executables as readonly
        let tx = Transaction::new(&[&keypair], message, Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx,
            &accounts,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        let result = loaded_accounts[0].0.as_ref().unwrap();
        assert_eq!(result.accounts[..2], accounts[..2]);
        assert_eq!(
            result.accounts[result.program_indices[0][0] as usize],
            accounts[5]
        );
        assert_eq!(
            result.accounts[result.program_indices[0][1] as usize],
            accounts[3]
        );
    }

    #[test]
    fn test_load_accounts_programdata_with_write_lock() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_counters = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);
        let key2 = Pubkey::from([6u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let program_data = UpgradeableLoaderState::ProgramData {
            slot: 42,
            upgrade_authority_address: None,
        };
        let mut account =
            AccountSharedData::new_data(40, &program_data, &bpf_loader_upgradeable::id()).unwrap();
        account.set_rent_epoch(1);
        accounts.push((key1, account));

        let mut account = AccountSharedData::new(40, 1, &native_loader::id());
        account.set_executable(true);
        account.set_rent_epoch(1);
        accounts.push((key2, account));

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let mut message = Message::new_with_compiled_instructions(
            1,
            0,
            1, // only the program marked as readonly
            vec![key0, key1, key2],
            Hash::default(),
            instructions,
        );
        let tx = Transaction::new(&[&keypair], message.clone(), Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx.clone(),
            &accounts,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            (Err(TransactionError::InvalidWritableAccount), None)
        );

        // Solution 0: Include feature simplify_writable_program_account_check
        let loaded_accounts =
            load_accounts_with_excluded_features(tx, &accounts, &mut error_counters, None);

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);

        // Solution 1: include bpf_loader_upgradeable account
        let mut account = AccountSharedData::new(40, 1, &native_loader::id()); // create mock bpf_loader_upgradeable
        account.set_executable(true);
        account.set_rent_epoch(1);
        let accounts_with_upgradeable_loader = vec![
            accounts[0].clone(),
            accounts[1].clone(),
            (bpf_loader_upgradeable::id(), account),
        ];
        message.account_keys = vec![key0, key1, bpf_loader_upgradeable::id()];
        let tx = Transaction::new(&[&keypair], message.clone(), Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx,
            &accounts_with_upgradeable_loader,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        let result = loaded_accounts[0].0.as_ref().unwrap();
        assert_eq!(result.accounts[..2], accounts_with_upgradeable_loader[..2]);
        assert_eq!(
            result.accounts[result.program_indices[0][0] as usize],
            accounts_with_upgradeable_loader[2]
        );

        // Solution 2: mark programdata as readonly
        message.account_keys = vec![key0, key1, key2]; // revert key change
        message.header.num_readonly_unsigned_accounts = 2; // extend readonly set to include programdata
        let tx = Transaction::new(&[&keypair], message, Hash::default());
        let loaded_accounts = load_accounts_with_excluded_features(
            tx,
            &accounts,
            &mut error_counters,
            Some(&[simplify_writable_program_account_check::id()]),
        );

        assert_eq!(error_counters.invalid_writable_account, 1);
        assert_eq!(loaded_accounts.len(), 1);
        let result = loaded_accounts[0].0.as_ref().unwrap();
        assert_eq!(result.accounts[..2], accounts[..2]);
        assert_eq!(
            result.accounts[result.program_indices[0][0] as usize],
            accounts[2]
        );
    }

    #[test]
    fn test_accounts_empty_bank_hash_stats() {
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        assert!(accounts.accounts_db.get_bank_hash_stats(0).is_some());
        assert!(accounts.accounts_db.get_bank_hash_stats(1).is_none());
    }

    #[test]
    fn test_lock_accounts_with_duplicates() {
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        let keypair = Keypair::new();
        let message = Message {
            header: MessageHeader {
                num_required_signatures: 1,
                ..MessageHeader::default()
            },
            account_keys: vec![keypair.pubkey(), keypair.pubkey()],
            ..Message::default()
        };

        let tx = new_sanitized_tx(&[&keypair], message, Hash::default());
        let results = accounts.lock_accounts([tx].iter(), MAX_TX_ACCOUNT_LOCKS);
        assert_eq!(results[0], Err(TransactionError::AccountLoadedTwice));
    }

    #[test]
    fn test_lock_accounts_with_too_many_accounts() {
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        let keypair = Keypair::new();

        // Allow up to MAX_TX_ACCOUNT_LOCKS
        {
            let num_account_keys = MAX_TX_ACCOUNT_LOCKS;
            let mut account_keys: Vec<_> = (0..num_account_keys)
                .map(|_| Pubkey::new_unique())
                .collect();
            account_keys[0] = keypair.pubkey();
            let message = Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    ..MessageHeader::default()
                },
                account_keys,
                ..Message::default()
            };

            let txs = vec![new_sanitized_tx(&[&keypair], message, Hash::default())];
            let results = accounts.lock_accounts(txs.iter(), MAX_TX_ACCOUNT_LOCKS);
            assert_eq!(results[0], Ok(()));
            accounts.unlock_accounts(txs.iter(), &results);
        }

        // Disallow over MAX_TX_ACCOUNT_LOCKS
        {
            let num_account_keys = MAX_TX_ACCOUNT_LOCKS + 1;
            let mut account_keys: Vec<_> = (0..num_account_keys)
                .map(|_| Pubkey::new_unique())
                .collect();
            account_keys[0] = keypair.pubkey();
            let message = Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    ..MessageHeader::default()
                },
                account_keys,
                ..Message::default()
            };

            let txs = vec![new_sanitized_tx(&[&keypair], message, Hash::default())];
            let results = accounts.lock_accounts(txs.iter(), MAX_TX_ACCOUNT_LOCKS);
            assert_eq!(results[0], Err(TransactionError::TooManyAccountLocks));
        }
    }

    #[test]
    fn test_accounts_locks() {
        let keypair0 = Keypair::new();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();

        let account0 = AccountSharedData::new(1, 0, &Pubkey::default());
        let account1 = AccountSharedData::new(2, 0, &Pubkey::default());
        let account2 = AccountSharedData::new(3, 0, &Pubkey::default());
        let account3 = AccountSharedData::new(4, 0, &Pubkey::default());

        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        accounts.store_for_tests(0, &keypair0.pubkey(), &account0);
        accounts.store_for_tests(0, &keypair1.pubkey(), &account1);
        accounts.store_for_tests(0, &keypair2.pubkey(), &account2);
        accounts.store_for_tests(0, &keypair3.pubkey(), &account3);

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair0.pubkey(), keypair1.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx = new_sanitized_tx(&[&keypair0], message, Hash::default());
        let results0 = accounts.lock_accounts([tx.clone()].iter(), MAX_TX_ACCOUNT_LOCKS);

        assert!(results0[0].is_ok());
        assert_eq!(
            *accounts
                .account_locks
                .lock()
                .unwrap()
                .readonly_locks
                .get(&keypair1.pubkey())
                .unwrap(),
            1
        );

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair2.pubkey(), keypair1.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx0 = new_sanitized_tx(&[&keypair2], message, Hash::default());
        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair1.pubkey(), keypair3.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx1 = new_sanitized_tx(&[&keypair1], message, Hash::default());
        let txs = vec![tx0, tx1];
        let results1 = accounts.lock_accounts(txs.iter(), MAX_TX_ACCOUNT_LOCKS);

        assert!(results1[0].is_ok()); // Read-only account (keypair1) can be referenced multiple times
        assert!(results1[1].is_err()); // Read-only account (keypair1) cannot also be locked as writable
        assert_eq!(
            *accounts
                .account_locks
                .lock()
                .unwrap()
                .readonly_locks
                .get(&keypair1.pubkey())
                .unwrap(),
            2
        );

        accounts.unlock_accounts([tx].iter(), &results0);
        accounts.unlock_accounts(txs.iter(), &results1);
        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair1.pubkey(), keypair3.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx = new_sanitized_tx(&[&keypair1], message, Hash::default());
        let results2 = accounts.lock_accounts([tx].iter(), MAX_TX_ACCOUNT_LOCKS);
        assert!(results2[0].is_ok()); // Now keypair1 account can be locked as writable

        // Check that read-only lock with zero references is deleted
        assert!(accounts
            .account_locks
            .lock()
            .unwrap()
            .readonly_locks
            .get(&keypair1.pubkey())
            .is_none());
    }

    #[test]
    fn test_accounts_locks_multithreaded() {
        let counter = Arc::new(AtomicU64::new(0));
        let exit = Arc::new(AtomicBool::new(false));

        let keypair0 = Keypair::new();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let account0 = AccountSharedData::new(1, 0, &Pubkey::default());
        let account1 = AccountSharedData::new(2, 0, &Pubkey::default());
        let account2 = AccountSharedData::new(3, 0, &Pubkey::default());

        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        accounts.store_for_tests(0, &keypair0.pubkey(), &account0);
        accounts.store_for_tests(0, &keypair1.pubkey(), &account1);
        accounts.store_for_tests(0, &keypair2.pubkey(), &account2);

        let accounts_arc = Arc::new(accounts);

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let readonly_message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair0.pubkey(), keypair1.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let readonly_tx = new_sanitized_tx(&[&keypair0], readonly_message, Hash::default());

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let writable_message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair1.pubkey(), keypair2.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let writable_tx = new_sanitized_tx(&[&keypair1], writable_message, Hash::default());

        let counter_clone = counter.clone();
        let accounts_clone = accounts_arc.clone();
        let exit_clone = exit.clone();
        thread::spawn(move || loop {
            let txs = vec![writable_tx.clone()];
            let results = accounts_clone
                .clone()
                .lock_accounts(txs.iter(), MAX_TX_ACCOUNT_LOCKS);
            for result in results.iter() {
                if result.is_ok() {
                    counter_clone.clone().fetch_add(1, Ordering::SeqCst);
                }
            }
            accounts_clone.unlock_accounts(txs.iter(), &results);
            if exit_clone.clone().load(Ordering::Relaxed) {
                break;
            }
        });
        let counter_clone = counter;
        for _ in 0..5 {
            let txs = vec![readonly_tx.clone()];
            let results = accounts_arc
                .clone()
                .lock_accounts(txs.iter(), MAX_TX_ACCOUNT_LOCKS);
            if results[0].is_ok() {
                let counter_value = counter_clone.clone().load(Ordering::SeqCst);
                thread::sleep(time::Duration::from_millis(50));
                assert_eq!(counter_value, counter_clone.clone().load(Ordering::SeqCst));
            }
            accounts_arc.unlock_accounts(txs.iter(), &results);
            thread::sleep(time::Duration::from_millis(50));
        }
        exit.store(true, Ordering::Relaxed);
    }

    #[test]
    fn test_demote_program_write_locks() {
        let keypair0 = Keypair::new();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();

        let account0 = AccountSharedData::new(1, 0, &Pubkey::default());
        let account1 = AccountSharedData::new(2, 0, &Pubkey::default());
        let account2 = AccountSharedData::new(3, 0, &Pubkey::default());
        let account3 = AccountSharedData::new(4, 0, &Pubkey::default());

        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        accounts.store_for_tests(0, &keypair0.pubkey(), &account0);
        accounts.store_for_tests(0, &keypair1.pubkey(), &account1);
        accounts.store_for_tests(0, &keypair2.pubkey(), &account2);
        accounts.store_for_tests(0, &keypair3.pubkey(), &account3);

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            0, // All accounts marked as writable
            vec![keypair0.pubkey(), keypair1.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx = new_sanitized_tx(&[&keypair0], message, Hash::default());
        let results0 = accounts.lock_accounts([tx].iter(), MAX_TX_ACCOUNT_LOCKS);

        assert!(results0[0].is_ok());
        // Instruction program-id account demoted to readonly
        assert_eq!(
            *accounts
                .account_locks
                .lock()
                .unwrap()
                .readonly_locks
                .get(&native_loader::id())
                .unwrap(),
            1
        );
        // Non-program accounts remain writable
        assert!(accounts
            .account_locks
            .lock()
            .unwrap()
            .write_locks
            .contains(&keypair0.pubkey()));
        assert!(accounts
            .account_locks
            .lock()
            .unwrap()
            .write_locks
            .contains(&keypair1.pubkey()));
    }

    impl Accounts {
        /// callers used to call store_uncached. But, this is not allowed anymore.
        pub fn store_for_tests(&self, slot: Slot, pubkey: &Pubkey, account: &AccountSharedData) {
            self.accounts_db.store_for_tests(slot, &[(pubkey, account)])
        }

        /// useful to adapt tests written prior to introduction of the write cache
        /// to use the write cache
        pub fn add_root_and_flush_write_cache(&self, slot: Slot) {
            self.add_root(slot);
            self.accounts_db.flush_accounts_cache_slot_for_tests(slot);
        }
    }

    #[test]
    fn test_accounts_locks_with_results() {
        let keypair0 = Keypair::new();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();

        let account0 = AccountSharedData::new(1, 0, &Pubkey::default());
        let account1 = AccountSharedData::new(2, 0, &Pubkey::default());
        let account2 = AccountSharedData::new(3, 0, &Pubkey::default());
        let account3 = AccountSharedData::new(4, 0, &Pubkey::default());

        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        accounts.store_for_tests(0, &keypair0.pubkey(), &account0);
        accounts.store_for_tests(0, &keypair1.pubkey(), &account1);
        accounts.store_for_tests(0, &keypair2.pubkey(), &account2);
        accounts.store_for_tests(0, &keypair3.pubkey(), &account3);

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair1.pubkey(), keypair0.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx0 = new_sanitized_tx(&[&keypair1], message, Hash::default());
        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair2.pubkey(), keypair0.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx1 = new_sanitized_tx(&[&keypair2], message, Hash::default());
        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair3.pubkey(), keypair0.pubkey(), native_loader::id()],
            Hash::default(),
            instructions,
        );
        let tx2 = new_sanitized_tx(&[&keypair3], message, Hash::default());
        let txs = vec![tx0, tx1, tx2];

        let qos_results = vec![
            Ok(()),
            Err(TransactionError::WouldExceedMaxBlockCostLimit),
            Ok(()),
        ];

        let results = accounts.lock_accounts_with_results(
            txs.iter(),
            qos_results.into_iter(),
            MAX_TX_ACCOUNT_LOCKS,
        );

        assert!(results[0].is_ok()); // Read-only account (keypair0) can be referenced multiple times
        assert!(results[1].is_err()); // is not locked due to !qos_results[1].is_ok()
        assert!(results[2].is_ok()); // Read-only account (keypair0) can be referenced multiple times

        // verify that keypair0 read-only lock twice (for tx0 and tx2)
        assert_eq!(
            *accounts
                .account_locks
                .lock()
                .unwrap()
                .readonly_locks
                .get(&keypair0.pubkey())
                .unwrap(),
            2
        );
        // verify that keypair2 (for tx1) is not write-locked
        assert!(accounts
            .account_locks
            .lock()
            .unwrap()
            .write_locks
            .get(&keypair2.pubkey())
            .is_none());

        accounts.unlock_accounts(txs.iter(), &results);

        // check all locks to be removed
        assert!(accounts
            .account_locks
            .lock()
            .unwrap()
            .readonly_locks
            .is_empty());
        assert!(accounts
            .account_locks
            .lock()
            .unwrap()
            .write_locks
            .is_empty());
    }

    #[test]
    fn test_collect_accounts_to_store() {
        let keypair0 = Keypair::new();
        let keypair1 = Keypair::new();
        let pubkey = solana_sdk::pubkey::new_rand();
        let account0 = AccountSharedData::new(1, 0, &Pubkey::default());
        let account1 = AccountSharedData::new(2, 0, &Pubkey::default());
        let account2 = AccountSharedData::new(3, 0, &Pubkey::default());

        let rent_collector = RentCollector::default();

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair0.pubkey(), pubkey, native_loader::id()],
            Hash::default(),
            instructions,
        );
        let transaction_accounts0 = vec![
            (message.account_keys[0], account0),
            (message.account_keys[1], account2.clone()),
        ];
        let tx0 = new_sanitized_tx(&[&keypair0], message, Hash::default());

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let message = Message::new_with_compiled_instructions(
            1,
            0,
            2,
            vec![keypair1.pubkey(), pubkey, native_loader::id()],
            Hash::default(),
            instructions,
        );
        let transaction_accounts1 = vec![
            (message.account_keys[0], account1),
            (message.account_keys[1], account2),
        ];
        let tx1 = new_sanitized_tx(&[&keypair1], message, Hash::default());

        let loaded0 = (
            Ok(LoadedTransaction {
                accounts: transaction_accounts0,
                program_indices: vec![],
                rent: 0,
                rent_debits: RentDebits::default(),
            }),
            None,
        );

        let loaded1 = (
            Ok(LoadedTransaction {
                accounts: transaction_accounts1,
                program_indices: vec![],
                rent: 0,
                rent_debits: RentDebits::default(),
            }),
            None,
        );

        let mut loaded = vec![loaded0, loaded1];

        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        {
            accounts
                .account_locks
                .lock()
                .unwrap()
                .insert_new_readonly(&pubkey);
        }
        let txs = vec![tx0.clone(), tx1.clone()];
        let execution_results = vec![new_execution_result(Ok(()), None); 2];
        let (collected_accounts, transactions) = accounts.collect_accounts_to_store(
            &txs,
            &execution_results,
            loaded.as_mut_slice(),
            &rent_collector,
            &DurableNonce::default(),
            0,
        );
        assert_eq!(collected_accounts.len(), 2);
        assert!(collected_accounts
            .iter()
            .any(|(pubkey, _account)| *pubkey == &keypair0.pubkey()));
        assert!(collected_accounts
            .iter()
            .any(|(pubkey, _account)| *pubkey == &keypair1.pubkey()));

        assert_eq!(transactions.len(), 2);
        assert!(transactions.iter().any(|txn| txn.unwrap().eq(&tx0)));
        assert!(transactions.iter().any(|txn| txn.unwrap().eq(&tx1)));

        // Ensure readonly_lock reflects lock
        assert_eq!(
            *accounts
                .account_locks
                .lock()
                .unwrap()
                .readonly_locks
                .get(&pubkey)
                .unwrap(),
            1
        );
    }

    #[test]
    fn huge_clean() {
        solana_logger::setup();
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        let mut old_pubkey = Pubkey::default();
        let zero_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
        info!("storing..");
        for i in 0..2_000 {
            let pubkey = solana_sdk::pubkey::new_rand();
            let account = AccountSharedData::new(i + 1, 0, AccountSharedData::default().owner());
            accounts.store_for_tests(i, &pubkey, &account);
            accounts.store_for_tests(i, &old_pubkey, &zero_account);
            old_pubkey = pubkey;
            accounts.add_root_and_flush_write_cache(i);

            if i % 1_000 == 0 {
                info!("  store {}", i);
            }
        }
        info!("done..cleaning..");
        accounts.accounts_db.clean_accounts_for_tests();
    }

    fn load_accounts_no_store(
        accounts: &Accounts,
        tx: Transaction,
        account_overrides: Option<&AccountOverrides>,
    ) -> Vec<TransactionLoadResult> {
        let tx = SanitizedTransaction::from_transaction_for_tests(tx);
        let rent_collector = RentCollector::default();
        let mut hash_queue = BlockhashQueue::new(100);
        hash_queue.register_hash(tx.message().recent_blockhash(), 10);

        let ancestors = vec![(0, 0)].into_iter().collect();
        let mut error_counters = TransactionErrorMetrics::default();
        accounts.load_accounts(
            &ancestors,
            &[tx],
            vec![(Ok(()), None)],
            &hash_queue,
            &mut error_counters,
            &rent_collector,
            &FeatureSet::all_enabled(),
            &FeeStructure::default(),
            account_overrides,
            RewardInterval::OutsideInterval,
            &HashMap::new(),
            &LoadedProgramsForTxBatch::default(),
        )
    }

    #[test]
    fn test_instructions() {
        solana_logger::setup();
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        let instructions_key = solana_sdk::sysvar::instructions::id();
        let keypair = Keypair::new();
        let instructions = vec![CompiledInstruction::new(1, &(), vec![0, 1])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[solana_sdk::pubkey::new_rand(), instructions_key],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts = load_accounts_no_store(&accounts, tx, None);
        assert_eq!(loaded_accounts.len(), 1);
        assert!(loaded_accounts[0].0.is_err());
    }

    #[test]
    fn test_overrides() {
        solana_logger::setup();
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        let mut account_overrides = AccountOverrides::default();
        let slot_history_id = sysvar::slot_history::id();
        let account = AccountSharedData::new(42, 0, &Pubkey::default());
        account_overrides.set_slot_history(Some(account));

        let keypair = Keypair::new();
        let account = AccountSharedData::new(1_000_000, 0, &Pubkey::default());
        accounts.store_slow_uncached(0, &keypair.pubkey(), &account);

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[slot_history_id],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts = load_accounts_no_store(&accounts, tx, Some(&account_overrides));
        assert_eq!(loaded_accounts.len(), 1);
        let loaded_transaction = loaded_accounts[0].0.as_ref().unwrap();
        assert_eq!(loaded_transaction.accounts[0].0, keypair.pubkey());
        assert_eq!(loaded_transaction.accounts[1].0, slot_history_id);
        assert_eq!(loaded_transaction.accounts[1].1.lamports(), 42);
    }

    fn create_accounts_prepare_if_nonce_account() -> (
        Pubkey,
        AccountSharedData,
        AccountSharedData,
        DurableNonce,
        u64,
        Option<AccountSharedData>,
    ) {
        let data = NonceVersions::new(NonceState::Initialized(nonce::state::Data::default()));
        let account = AccountSharedData::new_data(42, &data, &system_program::id()).unwrap();
        let mut pre_account = account.clone();
        pre_account.set_lamports(43);
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new(&[1u8; 32]));
        (
            Pubkey::default(),
            pre_account,
            account,
            durable_nonce,
            1234,
            None,
        )
    }

    fn run_prepare_if_nonce_account_test(
        account_address: &Pubkey,
        account: &mut AccountSharedData,
        tx_result: &Result<()>,
        is_fee_payer: bool,
        maybe_nonce: Option<(&NonceFull, bool)>,
        durable_nonce: &DurableNonce,
        lamports_per_signature: u64,
        expect_account: &AccountSharedData,
    ) -> bool {
        // Verify expect_account's relationship
        if !is_fee_payer {
            match maybe_nonce {
                Some((nonce, _)) if nonce.address() == account_address => {
                    assert_ne!(expect_account, nonce.account())
                }
                _ => assert_eq!(expect_account, account),
            }
        }

        prepare_if_nonce_account(
            account_address,
            account,
            tx_result,
            is_fee_payer,
            maybe_nonce,
            durable_nonce,
            lamports_per_signature,
        );
        assert_eq!(expect_account, account);
        expect_account == account
    }

    #[test]
    fn test_prepare_if_nonce_account_expected() {
        let (
            pre_account_address,
            pre_account,
            mut post_account,
            blockhash,
            lamports_per_signature,
            maybe_fee_payer_account,
        ) = create_accounts_prepare_if_nonce_account();
        let post_account_address = pre_account_address;
        let nonce = NonceFull::new(
            pre_account_address,
            pre_account.clone(),
            maybe_fee_payer_account,
        );

        let mut expect_account = pre_account;
        expect_account
            .set_state(&NonceVersions::new(NonceState::Initialized(
                nonce::state::Data::new(Pubkey::default(), blockhash, lamports_per_signature),
            )))
            .unwrap();

        assert!(run_prepare_if_nonce_account_test(
            &post_account_address,
            &mut post_account,
            &Ok(()),
            false,
            Some((&nonce, true)),
            &blockhash,
            lamports_per_signature,
            &expect_account,
        ));
    }

    #[test]
    fn test_prepare_if_nonce_account_not_nonce_tx() {
        let (
            pre_account_address,
            _pre_account,
            _post_account,
            blockhash,
            lamports_per_signature,
            _maybe_fee_payer_account,
        ) = create_accounts_prepare_if_nonce_account();
        let post_account_address = pre_account_address;

        let mut post_account = AccountSharedData::default();
        let expect_account = post_account.clone();
        assert!(run_prepare_if_nonce_account_test(
            &post_account_address,
            &mut post_account,
            &Ok(()),
            false,
            None,
            &blockhash,
            lamports_per_signature,
            &expect_account,
        ));
    }

    #[test]
    fn test_prepare_if_nonce_account_not_nonce_address() {
        let (
            pre_account_address,
            pre_account,
            mut post_account,
            blockhash,
            lamports_per_signature,
            maybe_fee_payer_account,
        ) = create_accounts_prepare_if_nonce_account();

        let nonce = NonceFull::new(pre_account_address, pre_account, maybe_fee_payer_account);

        let expect_account = post_account.clone();
        // Wrong key
        assert!(run_prepare_if_nonce_account_test(
            &Pubkey::from([1u8; 32]),
            &mut post_account,
            &Ok(()),
            false,
            Some((&nonce, true)),
            &blockhash,
            lamports_per_signature,
            &expect_account,
        ));
    }

    #[test]
    fn test_prepare_if_nonce_account_tx_error() {
        let (
            pre_account_address,
            pre_account,
            mut post_account,
            blockhash,
            lamports_per_signature,
            maybe_fee_payer_account,
        ) = create_accounts_prepare_if_nonce_account();
        let post_account_address = pre_account_address;
        let mut expect_account = pre_account.clone();

        let nonce = NonceFull::new(pre_account_address, pre_account, maybe_fee_payer_account);

        expect_account
            .set_state(&NonceVersions::new(NonceState::Initialized(
                nonce::state::Data::new(Pubkey::default(), blockhash, lamports_per_signature),
            )))
            .unwrap();

        assert!(run_prepare_if_nonce_account_test(
            &post_account_address,
            &mut post_account,
            &Err(TransactionError::InstructionError(
                0,
                InstructionError::InvalidArgument,
            )),
            false,
            Some((&nonce, true)),
            &blockhash,
            lamports_per_signature,
            &expect_account,
        ));
    }

    #[test]
    fn test_rollback_nonce_fee_payer() {
        let nonce_account = AccountSharedData::new_data(1, &(), &system_program::id()).unwrap();
        let pre_fee_payer_account =
            AccountSharedData::new_data(42, &(), &system_program::id()).unwrap();
        let mut post_fee_payer_account =
            AccountSharedData::new_data(84, &[1, 2, 3, 4], &system_program::id()).unwrap();
        let nonce = NonceFull::new(
            Pubkey::new_unique(),
            nonce_account,
            Some(pre_fee_payer_account.clone()),
        );

        assert!(run_prepare_if_nonce_account_test(
            &Pubkey::new_unique(),
            &mut post_fee_payer_account.clone(),
            &Err(TransactionError::InstructionError(
                0,
                InstructionError::InvalidArgument,
            )),
            false,
            Some((&nonce, true)),
            &DurableNonce::default(),
            1,
            &post_fee_payer_account,
        ));

        assert!(run_prepare_if_nonce_account_test(
            &Pubkey::new_unique(),
            &mut post_fee_payer_account.clone(),
            &Ok(()),
            true,
            Some((&nonce, true)),
            &DurableNonce::default(),
            1,
            &post_fee_payer_account,
        ));

        assert!(run_prepare_if_nonce_account_test(
            &Pubkey::new_unique(),
            &mut post_fee_payer_account.clone(),
            &Err(TransactionError::InstructionError(
                0,
                InstructionError::InvalidArgument,
            )),
            true,
            None,
            &DurableNonce::default(),
            1,
            &post_fee_payer_account,
        ));

        assert!(run_prepare_if_nonce_account_test(
            &Pubkey::new_unique(),
            &mut post_fee_payer_account,
            &Err(TransactionError::InstructionError(
                0,
                InstructionError::InvalidArgument,
            )),
            true,
            Some((&nonce, true)),
            &DurableNonce::default(),
            1,
            &pre_fee_payer_account,
        ));
    }

    #[test]
    fn test_nonced_failure_accounts_rollback_from_pays() {
        let rent_collector = RentCollector::default();

        let nonce_address = Pubkey::new_unique();
        let nonce_authority = keypair_from_seed(&[0; 32]).unwrap();
        let from = keypair_from_seed(&[1; 32]).unwrap();
        let from_address = from.pubkey();
        let to_address = Pubkey::new_unique();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_state = NonceVersions::new(NonceState::Initialized(nonce::state::Data::new(
            nonce_authority.pubkey(),
            durable_nonce,
            0,
        )));
        let nonce_account_post =
            AccountSharedData::new_data(43, &nonce_state, &system_program::id()).unwrap();
        let from_account_post = AccountSharedData::new(4199, 0, &Pubkey::default());
        let to_account = AccountSharedData::new(2, 0, &Pubkey::default());
        let nonce_authority_account = AccountSharedData::new(3, 0, &Pubkey::default());
        let recent_blockhashes_sysvar_account = AccountSharedData::new(4, 0, &Pubkey::default());

        let instructions = vec![
            system_instruction::advance_nonce_account(&nonce_address, &nonce_authority.pubkey()),
            system_instruction::transfer(&from_address, &to_address, 42),
        ];
        let message = Message::new(&instructions, Some(&from_address));
        let blockhash = Hash::new_unique();
        let transaction_accounts = vec![
            (message.account_keys[0], from_account_post),
            (message.account_keys[1], nonce_authority_account),
            (message.account_keys[2], nonce_account_post),
            (message.account_keys[3], to_account),
            (message.account_keys[4], recent_blockhashes_sysvar_account),
        ];
        let tx = new_sanitized_tx(&[&nonce_authority, &from], message, blockhash);

        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_state = NonceVersions::new(NonceState::Initialized(nonce::state::Data::new(
            nonce_authority.pubkey(),
            durable_nonce,
            0,
        )));
        let nonce_account_pre =
            AccountSharedData::new_data(42, &nonce_state, &system_program::id()).unwrap();
        let from_account_pre = AccountSharedData::new(4242, 0, &Pubkey::default());

        let nonce = Some(NonceFull::new(
            nonce_address,
            nonce_account_pre.clone(),
            Some(from_account_pre.clone()),
        ));

        let loaded = (
            Ok(LoadedTransaction {
                accounts: transaction_accounts,
                program_indices: vec![],
                rent: 0,
                rent_debits: RentDebits::default(),
            }),
            nonce.clone(),
        );

        let mut loaded = vec![loaded];

        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        let txs = vec![tx];
        let execution_results = vec![new_execution_result(
            Err(TransactionError::InstructionError(
                1,
                InstructionError::InvalidArgument,
            )),
            nonce.as_ref(),
        )];
        let (collected_accounts, _) = accounts.collect_accounts_to_store(
            &txs,
            &execution_results,
            loaded.as_mut_slice(),
            &rent_collector,
            &durable_nonce,
            0,
        );
        assert_eq!(collected_accounts.len(), 2);
        assert_eq!(
            collected_accounts
                .iter()
                .find(|(pubkey, _account)| *pubkey == &from_address)
                .map(|(_pubkey, account)| *account)
                .cloned()
                .unwrap(),
            from_account_pre,
        );
        let collected_nonce_account = collected_accounts
            .iter()
            .find(|(pubkey, _account)| *pubkey == &nonce_address)
            .map(|(_pubkey, account)| *account)
            .cloned()
            .unwrap();
        assert_eq!(
            collected_nonce_account.lamports(),
            nonce_account_pre.lamports(),
        );
        assert_matches!(
            nonce_account::verify_nonce_account(&collected_nonce_account, durable_nonce.as_hash()),
            Some(_)
        );
    }

    #[test]
    fn test_nonced_failure_accounts_rollback_nonce_pays() {
        let rent_collector = RentCollector::default();

        let nonce_authority = keypair_from_seed(&[0; 32]).unwrap();
        let nonce_address = nonce_authority.pubkey();
        let from = keypair_from_seed(&[1; 32]).unwrap();
        let from_address = from.pubkey();
        let to_address = Pubkey::new_unique();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_state = NonceVersions::new(NonceState::Initialized(nonce::state::Data::new(
            nonce_authority.pubkey(),
            durable_nonce,
            0,
        )));
        let nonce_account_post =
            AccountSharedData::new_data(43, &nonce_state, &system_program::id()).unwrap();
        let from_account_post = AccountSharedData::new(4200, 0, &Pubkey::default());
        let to_account = AccountSharedData::new(2, 0, &Pubkey::default());
        let nonce_authority_account = AccountSharedData::new(3, 0, &Pubkey::default());
        let recent_blockhashes_sysvar_account = AccountSharedData::new(4, 0, &Pubkey::default());

        let instructions = vec![
            system_instruction::advance_nonce_account(&nonce_address, &nonce_authority.pubkey()),
            system_instruction::transfer(&from_address, &to_address, 42),
        ];
        let message = Message::new(&instructions, Some(&nonce_address));
        let blockhash = Hash::new_unique();
        let transaction_accounts = vec![
            (message.account_keys[0], from_account_post),
            (message.account_keys[1], nonce_authority_account),
            (message.account_keys[2], nonce_account_post),
            (message.account_keys[3], to_account),
            (message.account_keys[4], recent_blockhashes_sysvar_account),
        ];
        let tx = new_sanitized_tx(&[&nonce_authority, &from], message, blockhash);

        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_state = NonceVersions::new(NonceState::Initialized(nonce::state::Data::new(
            nonce_authority.pubkey(),
            durable_nonce,
            0,
        )));
        let nonce_account_pre =
            AccountSharedData::new_data(42, &nonce_state, &system_program::id()).unwrap();

        let nonce = Some(NonceFull::new(
            nonce_address,
            nonce_account_pre.clone(),
            None,
        ));

        let loaded = (
            Ok(LoadedTransaction {
                accounts: transaction_accounts,
                program_indices: vec![],
                rent: 0,
                rent_debits: RentDebits::default(),
            }),
            nonce.clone(),
        );

        let mut loaded = vec![loaded];

        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );
        let txs = vec![tx];
        let execution_results = vec![new_execution_result(
            Err(TransactionError::InstructionError(
                1,
                InstructionError::InvalidArgument,
            )),
            nonce.as_ref(),
        )];
        let (collected_accounts, _) = accounts.collect_accounts_to_store(
            &txs,
            &execution_results,
            loaded.as_mut_slice(),
            &rent_collector,
            &durable_nonce,
            0,
        );
        assert_eq!(collected_accounts.len(), 1);
        let collected_nonce_account = collected_accounts
            .iter()
            .find(|(pubkey, _account)| *pubkey == &nonce_address)
            .map(|(_pubkey, account)| *account)
            .cloned()
            .unwrap();
        assert_eq!(
            collected_nonce_account.lamports(),
            nonce_account_pre.lamports()
        );
        assert_matches!(
            nonce_account::verify_nonce_account(&collected_nonce_account, durable_nonce.as_hash()),
            Some(_)
        );
    }

    #[test]
    fn test_load_largest_accounts() {
        let accounts = Accounts::new_with_config_for_tests(
            Vec::new(),
            &ClusterType::Development,
            AccountSecondaryIndexes::default(),
            AccountShrinkThreshold::default(),
        );

        /* This test assumes pubkey0 < pubkey1 < pubkey2.
         * But the keys created with new_unique() does not gurantee this
         * order because of the endianness.  new_unique() calls add 1 at each
         * key generaration as the little endian integer.  A pubkey stores its
         * value in a 32-byte array bytes, and its eq-partial trait considers
         * the lower-address bytes more significant, which is the big-endian
         * order.
         * So, sort first to ensure the order assumption holds.
         */
        let mut keys = vec![];
        for _idx in 0..3 {
            keys.push(Pubkey::new_unique());
        }
        keys.sort();
        let pubkey2 = keys.pop().unwrap();
        let pubkey1 = keys.pop().unwrap();
        let pubkey0 = keys.pop().unwrap();
        let account0 = AccountSharedData::new(42, 0, &Pubkey::default());
        accounts.store_for_tests(0, &pubkey0, &account0);
        let account1 = AccountSharedData::new(42, 0, &Pubkey::default());
        accounts.store_for_tests(0, &pubkey1, &account1);
        let account2 = AccountSharedData::new(41, 0, &Pubkey::default());
        accounts.store_for_tests(0, &pubkey2, &account2);

        let ancestors = vec![(0, 0)].into_iter().collect();
        let all_pubkeys: HashSet<_> = vec![pubkey0, pubkey1, pubkey2].into_iter().collect();

        // num == 0 should always return empty set
        let bank_id = 0;
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    0,
                    &HashSet::new(),
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![]
        );
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    0,
                    &all_pubkeys,
                    AccountAddressFilter::Include
                )
                .unwrap(),
            vec![]
        );

        // list should be sorted by balance, then pubkey, descending
        assert!(pubkey1 > pubkey0);
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    1,
                    &HashSet::new(),
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![(pubkey1, 42)]
        );
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    2,
                    &HashSet::new(),
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![(pubkey1, 42), (pubkey0, 42)]
        );
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    3,
                    &HashSet::new(),
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![(pubkey1, 42), (pubkey0, 42), (pubkey2, 41)]
        );

        // larger num should not affect results
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    6,
                    &HashSet::new(),
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![(pubkey1, 42), (pubkey0, 42), (pubkey2, 41)]
        );

        // AccountAddressFilter::Exclude should exclude entry
        let exclude1: HashSet<_> = vec![pubkey1].into_iter().collect();
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    1,
                    &exclude1,
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![(pubkey0, 42)]
        );
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    2,
                    &exclude1,
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![(pubkey0, 42), (pubkey2, 41)]
        );
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    3,
                    &exclude1,
                    AccountAddressFilter::Exclude
                )
                .unwrap(),
            vec![(pubkey0, 42), (pubkey2, 41)]
        );

        // AccountAddressFilter::Include should limit entries
        let include1_2: HashSet<_> = vec![pubkey1, pubkey2].into_iter().collect();
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    1,
                    &include1_2,
                    AccountAddressFilter::Include
                )
                .unwrap(),
            vec![(pubkey1, 42)]
        );
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    2,
                    &include1_2,
                    AccountAddressFilter::Include
                )
                .unwrap(),
            vec![(pubkey1, 42), (pubkey2, 41)]
        );
        assert_eq!(
            accounts
                .load_largest_accounts(
                    &ancestors,
                    bank_id,
                    3,
                    &include1_2,
                    AccountAddressFilter::Include
                )
                .unwrap(),
            vec![(pubkey1, 42), (pubkey2, 41)]
        );
    }

    fn zero_len_account_size() -> usize {
        std::mem::size_of::<AccountSharedData>() + std::mem::size_of::<Pubkey>()
    }

    #[test]
    fn test_calc_scan_result_size() {
        for len in 0..3 {
            assert_eq!(
                Accounts::calc_scan_result_size(&AccountSharedData::new(
                    0,
                    len,
                    &Pubkey::default()
                )),
                zero_len_account_size() + len
            );
        }
    }

    #[test]
    fn test_maybe_abort_scan() {
        assert!(Accounts::maybe_abort_scan(ScanResult::Ok(vec![]), &ScanConfig::default()).is_ok());
        let config = ScanConfig::default().recreate_with_abort();
        assert!(Accounts::maybe_abort_scan(ScanResult::Ok(vec![]), &config).is_ok());
        config.abort();
        assert!(Accounts::maybe_abort_scan(ScanResult::Ok(vec![]), &config).is_err());
    }

    #[test]
    fn test_accumulate_and_check_scan_result_size() {
        for (account, byte_limit_for_scan, result) in [
            (AccountSharedData::default(), zero_len_account_size(), false),
            (
                AccountSharedData::new(0, 1, &Pubkey::default()),
                zero_len_account_size(),
                true,
            ),
            (
                AccountSharedData::new(0, 2, &Pubkey::default()),
                zero_len_account_size() + 3,
                false,
            ),
        ] {
            let sum = AtomicUsize::default();
            assert_eq!(
                result,
                Accounts::accumulate_and_check_scan_result_size(
                    &sum,
                    &account,
                    &Some(byte_limit_for_scan)
                )
            );
            // calling a second time should accumulate above the threshold
            assert!(Accounts::accumulate_and_check_scan_result_size(
                &sum,
                &account,
                &Some(byte_limit_for_scan)
            ));
            assert!(!Accounts::accumulate_and_check_scan_result_size(
                &sum, &account, &None
            ));
        }
    }

    #[test]
    fn test_accumulate_and_check_loaded_account_data_size() {
        let mut error_counter = TransactionErrorMetrics::default();

        // assert check is OK if data limit is not enabled
        {
            let mut accumulated_data_size: usize = 0;
            let data_size = usize::MAX;
            let requested_data_size_limit = None;

            assert!(Accounts::accumulate_and_check_loaded_account_data_size(
                &mut accumulated_data_size,
                data_size,
                requested_data_size_limit,
                &mut error_counter
            )
            .is_ok());
        }

        // assert check will fail with correct error if loaded data exceeds limit
        {
            let mut accumulated_data_size: usize = 0;
            let data_size: usize = 123;
            let requested_data_size_limit = NonZeroUsize::new(data_size);

            // OK - loaded data size is up to limit
            assert!(Accounts::accumulate_and_check_loaded_account_data_size(
                &mut accumulated_data_size,
                data_size,
                requested_data_size_limit,
                &mut error_counter
            )
            .is_ok());
            assert_eq!(data_size, accumulated_data_size);

            // fail - loading more data that would exceed limit
            let another_byte: usize = 1;
            assert_eq!(
                Accounts::accumulate_and_check_loaded_account_data_size(
                    &mut accumulated_data_size,
                    another_byte,
                    requested_data_size_limit,
                    &mut error_counter
                ),
                Err(TransactionError::MaxLoadedAccountsDataSizeExceeded)
            );
        }
    }

    #[test]
    fn test_get_requested_loaded_accounts_data_size_limit() {
        // an prrivate helper function
        fn test(
            instructions: &[solana_sdk::instruction::Instruction],
            feature_set: &FeatureSet,
            expected_result: &Result<Option<NonZeroUsize>>,
        ) {
            let payer_keypair = Keypair::new();
            let tx = SanitizedTransaction::from_transaction_for_tests(Transaction::new(
                &[&payer_keypair],
                Message::new(instructions, Some(&payer_keypair.pubkey())),
                Hash::default(),
            ));
            assert_eq!(
                *expected_result,
                Accounts::get_requested_loaded_accounts_data_size_limit(&tx, feature_set)
            );
        }

        let tx_not_set_limit = &[solana_sdk::instruction::Instruction::new_with_bincode(
            Pubkey::new_unique(),
            &0_u8,
            vec![],
        )];
        let tx_set_limit_99 =
                &[
                    solana_sdk::compute_budget::ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(99u32),
                    solana_sdk::instruction::Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ];
        let tx_set_limit_0 =
                &[
                    solana_sdk::compute_budget::ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(0u32),
                    solana_sdk::instruction::Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ];

        let result_no_limit = Ok(None);
        let result_default_limit = Ok(Some(
            NonZeroUsize::new(compute_budget::MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES).unwrap(),
        ));
        let result_requested_limit: Result<Option<NonZeroUsize>> =
            Ok(Some(NonZeroUsize::new(99).unwrap()));
        let result_invalid_limit = Err(TransactionError::InvalidLoadedAccountsDataSizeLimit);

        let mut feature_set = FeatureSet::default();

        // if `cap_transaction_accounts_data_size feature` is disable,
        // the result will always be no limit
        test(tx_not_set_limit, &feature_set, &result_no_limit);
        test(tx_set_limit_99, &feature_set, &result_no_limit);
        test(tx_set_limit_0, &feature_set, &result_no_limit);

        // if `cap_transaction_accounts_data_size` is enabled, and
        //    `add_set_tx_loaded_accounts_data_size_instruction` is disabled,
        // the result will always be default limit (64MiB)
        feature_set.activate(&feature_set::cap_transaction_accounts_data_size::id(), 0);
        test(tx_not_set_limit, &feature_set, &result_default_limit);
        test(tx_set_limit_99, &feature_set, &result_default_limit);
        test(tx_set_limit_0, &feature_set, &result_default_limit);

        // if `cap_transaction_accounts_data_size` and
        //    `add_set_tx_loaded_accounts_data_size_instruction` are both enabled,
        // the results are:
        //    if tx doesn't set limit, then default limit (64MiB)
        //    if tx sets limit, then requested limit
        //    if tx sets limit to zero, then TransactionError::InvalidLoadedAccountsDataSizeLimit
        feature_set.activate(&add_set_tx_loaded_accounts_data_size_instruction::id(), 0);
        test(tx_not_set_limit, &feature_set, &result_default_limit);
        test(tx_set_limit_99, &feature_set, &result_requested_limit);
        test(tx_set_limit_0, &feature_set, &result_invalid_limit);
    }

    #[test]
    fn test_load_accounts_too_high_prioritization_fee() {
        solana_logger::setup();
        let lamports_per_signature = 5000_u64;
        let request_units = 1_000_000_u32;
        let request_unit_price = 2_000_000_000_u64;
        let prioritization_fee_details = PrioritizationFeeDetails::new(
            PrioritizationFeeType::ComputeUnitPrice(request_unit_price),
            request_units as u64,
        );
        let prioritization_fee = prioritization_fee_details.get_fee();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        // set up account with balance of `prioritization_fee`
        let account = AccountSharedData::new(prioritization_fee, 0, &Pubkey::default());
        let accounts = vec![(key0, account)];

        let instructions = &[
            ComputeBudgetInstruction::set_compute_unit_limit(request_units),
            ComputeBudgetInstruction::set_compute_unit_price(request_unit_price),
        ];
        let tx = Transaction::new(
            &[&keypair],
            Message::new(instructions, Some(&key0)),
            Hash::default(),
        );

        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&remove_deprecated_request_unit_ix::id());

        let message = SanitizedMessage::try_from(tx.message().clone()).unwrap();
        let fee = FeeStructure::default().calculate_fee(
            &message,
            lamports_per_signature,
            &ComputeBudget::fee_budget_limits(message.program_instructions_iter(), &feature_set),
            true,
            false,
        );
        assert_eq!(fee, lamports_per_signature + prioritization_fee);

        // assert fail to load account with 2B lamport balance for transaction asking for 2B
        // lamports as prioritization fee.
        let mut error_counters = TransactionErrorMetrics::default();
        let loaded_accounts = load_accounts_with_fee(
            tx,
            &accounts,
            lamports_per_signature,
            &mut error_counters,
            None,
        );

        assert_eq!(error_counters.insufficient_funds, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0].clone(),
            (Err(TransactionError::InsufficientFundsForFee), None),
        );
    }

    struct ValidateFeePayerTestParameter {
        is_nonce: bool,
        payer_init_balance: u64,
        fee: u64,
        expected_result: Result<()>,
        payer_post_balance: u64,
        feature_checked_arithmmetic_enable: bool,
    }

    fn validate_fee_payer_account(
        test_parameter: ValidateFeePayerTestParameter,
        rent_collector: &RentCollector,
    ) {
        let payer_account_keys = Keypair::new();
        let mut account = if test_parameter.is_nonce {
            AccountSharedData::new_data(
                test_parameter.payer_init_balance,
                &NonceVersions::new(NonceState::Initialized(nonce::state::Data::default())),
                &system_program::id(),
            )
            .unwrap()
        } else {
            AccountSharedData::new(test_parameter.payer_init_balance, 0, &system_program::id())
        };
        let mut feature_set = FeatureSet::default();
        if test_parameter.feature_checked_arithmmetic_enable {
            feature_set.activate(&feature_set::checked_arithmetic_in_fee_validation::id(), 0);
        };
        let result = Accounts::validate_fee_payer(
            &payer_account_keys.pubkey(),
            &mut account,
            0,
            &mut TransactionErrorMetrics::default(),
            rent_collector,
            &feature_set,
            test_parameter.fee,
        );

        assert_eq!(result, test_parameter.expected_result);
        assert_eq!(account.lamports(), test_parameter.payer_post_balance);
    }

    #[test]
    fn test_validate_fee_payer() {
        let rent_collector = RentCollector::new(
            0,
            EpochSchedule::default(),
            500_000.0,
            Rent {
                lamports_per_byte_year: 1,
                ..Rent::default()
            },
        );
        let min_balance = rent_collector.rent.minimum_balance(NonceState::size());
        let fee = 5_000;

        // If payer account has sufficient balance, expect successful fee deduction,
        // regardless feature gate status, or if payer is nonce account.
        {
            for feature_checked_arithmmetic_enable in [true, false] {
                for (is_nonce, min_balance) in [(true, min_balance), (false, 0)] {
                    validate_fee_payer_account(
                        ValidateFeePayerTestParameter {
                            is_nonce,
                            payer_init_balance: min_balance + fee,
                            fee,
                            expected_result: Ok(()),
                            payer_post_balance: min_balance,
                            feature_checked_arithmmetic_enable,
                        },
                        &rent_collector,
                    );
                }
            }
        }

        // If payer account has no balance, expected AccountNotFound Error
        // regardless feature gate status, or if payer is nonce account.
        {
            for feature_checked_arithmmetic_enable in [true, false] {
                for is_nonce in [true, false] {
                    validate_fee_payer_account(
                        ValidateFeePayerTestParameter {
                            is_nonce,
                            payer_init_balance: 0,
                            fee,
                            expected_result: Err(TransactionError::AccountNotFound),
                            payer_post_balance: 0,
                            feature_checked_arithmmetic_enable,
                        },
                        &rent_collector,
                    );
                }
            }
        }

        // If payer account has insufficent balance, expect InsufficientFundsForFee error
        // regardless feature gate status, or if payer is nonce account.
        {
            for feature_checked_arithmmetic_enable in [true, false] {
                for (is_nonce, min_balance) in [(true, min_balance), (false, 0)] {
                    validate_fee_payer_account(
                        ValidateFeePayerTestParameter {
                            is_nonce,
                            payer_init_balance: min_balance + fee - 1,
                            fee,
                            expected_result: Err(TransactionError::InsufficientFundsForFee),
                            payer_post_balance: min_balance + fee - 1,
                            feature_checked_arithmmetic_enable,
                        },
                        &rent_collector,
                    );
                }
            }
        }

        // normal payer account has balance of u64::MAX, so does fee; since it does not  require
        // min_balance, expect successful fee deduction, regardless of feature gate status
        {
            for feature_checked_arithmmetic_enable in [true, false] {
                validate_fee_payer_account(
                    ValidateFeePayerTestParameter {
                        is_nonce: false,
                        payer_init_balance: u64::MAX,
                        fee: u64::MAX,
                        expected_result: Ok(()),
                        payer_post_balance: 0,
                        feature_checked_arithmmetic_enable,
                    },
                    &rent_collector,
                );
            }
        }
    }

    #[test]
    fn test_validate_nonce_fee_payer_with_checked_arithmetic() {
        let rent_collector = RentCollector::new(
            0,
            EpochSchedule::default(),
            500_000.0,
            Rent {
                lamports_per_byte_year: 1,
                ..Rent::default()
            },
        );

        // nonce payer account has balance of u64::MAX, so does fee; due to nonce account
        // requires additional min_balance, expect InsufficientFundsForFee error if feature gate is
        // enabled
        validate_fee_payer_account(
            ValidateFeePayerTestParameter {
                is_nonce: true,
                payer_init_balance: u64::MAX,
                fee: u64::MAX,
                expected_result: Err(TransactionError::InsufficientFundsForFee),
                payer_post_balance: u64::MAX,
                feature_checked_arithmmetic_enable: true,
            },
            &rent_collector,
        );
    }

    #[test]
    #[should_panic]
    fn test_validate_nonce_fee_payer_without_checked_arithmetic() {
        let rent_collector = RentCollector::new(
            0,
            EpochSchedule::default(),
            500_000.0,
            Rent {
                lamports_per_byte_year: 1,
                ..Rent::default()
            },
        );

        // same test setup as `test_validate_nonce_fee_payer_with_checked_arithmetic`:
        // nonce payer account has balance of u64::MAX, so does fee; and nonce account
        // requires additional min_balance, if feature gate is not enabled, in `debug`
        // mode, `u64::MAX + min_balance` would panic on "attempt to add with overflow";
        // in `release` mode, the addition will wrap, so the expected result would be
        // `Ok(())` with post payer balance `0`, therefore fails test with a panic.
        validate_fee_payer_account(
            ValidateFeePayerTestParameter {
                is_nonce: true,
                payer_init_balance: u64::MAX,
                fee: u64::MAX,
                expected_result: Err(TransactionError::InsufficientFundsForFee),
                payer_post_balance: u64::MAX,
                feature_checked_arithmmetic_enable: false,
            },
            &rent_collector,
        );
    }
}
