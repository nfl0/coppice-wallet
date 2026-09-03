use zcash_client_backend::{
    data_api::{
        chain::{scan_cached_blocks, CommitmentTreeRoot},
        scanning::ScanPriority,
        WalletCommitmentTrees, WalletRead, WalletWrite,
    },
    proto::service::TreeState,
};
use zcash_client_sqlite::{
    chain::{init::init_blockmeta_db, BlockMeta},
    AccountUuid, FsBlockDb,
};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::BlockHeight;

use crate::wallet::{
    db::{
        open_readonly_conn_with_timeout, open_wallet_db_for_read_with_timeout,
        open_wallet_db_with_timeout, with_wallet_db_write_lock, WalletDatabase,
        READ_DB_BUSY_TIMEOUT, WALLET_DB_BUSY_TIMEOUT,
    },
    network::WalletNetwork,
};

mod broadcast;
mod migration;
mod migration_wallet_ops;
mod pczt;
mod proposal_locks;
mod send;
mod transactions;

// Re-export the split submodules at the `wallet::sync` path so every
// `crate::wallet::sync::propose_send` / `::get_wallet_balance` /
// `::extract_and_broadcast_pczt` etc. call path keeps resolving with
// the same visibility the monolithic `sync.rs` had before the refactor.
// Functions were `pub fn` in the old file → `pub use`. Return-value
// structs were `pub(crate) struct` → `pub(crate) use` (they're
// reachable from anywhere in the crate but not re-exported to
// downstream consumers, which matches the pre-refactor surface
// exactly).
pub(crate) use migration::{
    configure_fast_testnet_migration, delete_account_migration_rows_with_tx,
    denomination_confirmations_required, migration_preparation_snapshot_read_only,
    migration_status, observable_denomination_transaction_ids, proof_retry_height,
    reconcile_wallet_locks_after_sync, MigrationPartState, MigrationPreparationOutputKind,
    MigrationPreparationTransactionState, MigrationScheduleEntry, MigrationStatus,
    PreparationTimingPolicy,
};
pub(crate) use pczt::extract_compact_sigs_from_pczt;
pub use pczt::{
    add_proofs_to_pczt, create_pczt_from_proposal, create_tex_pczts_from_proposal,
    discard_proposal, extract_and_broadcast_pczt, redact_pczt_for_signer,
    retain_proposal_lock_until_expiry, start_orchard_proving_key_warmup,
    store_and_broadcast_signed_pczts_for_proposal, ExtractAndBroadcastPcztResult,
    StoreAndBroadcastPcztsResult, TexPcztPair,
};
pub(crate) use proposal_locks::recover_previous_process as recover_orphaned_send_locks;
pub(crate) use send::propose_send;
pub(crate) use send::propose_send_with_raw_memo;
pub(crate) use send::{
    abandon_orchard_migration, advance_orchard_migration_preparation_for_run,
    complete_orchard_migration_batch_pczt, complete_orchard_migration_denominations_pczt,
    complete_orchard_migration_immediate_pczt, complete_orchard_migration_single_qr_pczt,
    create_or_resume_private_migration_draft, discard_all_keystone_migration_requests,
    discard_keystone_migration_request, discard_keystone_migration_requests_for_account,
    keystone_migration_proof_status, migrate_orchard_to_ironwood,
    migrate_orchard_to_ironwood_immediately, orchard_migration_proof_readiness,
    orchard_migration_proof_readiness_at_scanned_height,
    orchard_migration_proof_readiness_read_only, prepare_orchard_migration_batch_pczt,
    prepare_orchard_migration_denominations_pczt, prepare_orchard_migration_immediate_pczt,
    prepare_orchard_migration_single_qr_pczt, retain_migration_anchor_checkpoints_before_scan,
    retain_prepared_note_anchor_checkpoints_after_scan, retire_unbroadcast_orchard_migration,
    KeystoneSignedMigrationMessage, OrchardMigrationImmediatePlan,
};
pub use send::{
    broadcast_due_orchard_migration_transactions, broadcast_one_due_orchard_migration_transaction,
    estimate_fee, execute_proposal, execute_proposal_with_seed_loader, ExecuteProposalResult,
    IronwoodMigrationResult,
};
pub(crate) use send::{broadcast_raw_transaction_isolated, estimate_send_max};
pub(crate) use send::{
    create_shield_transparent_pczt, get_shield_transparent_status, shield_transparent_balance,
};
pub(crate) use send::{get_orchard_migration_immediate_plan, get_orchard_migration_private_plan};
// Internal-only re-export for `sync_engine::run_sync_impl`'s
// auto-resubmit pass. Not part of the `wallet::sync` public surface.
pub(crate) use send::migration_anchor_retention_required;
pub(crate) use send::resubmit_pending_transactions;
#[allow(unused_imports)] // names reachable via `crate::wallet::sync::*`; pre-refactor surface
pub(crate) use send::ProposalResult;
#[allow(unused_imports)] // names reachable via `crate::wallet::sync::*`; pre-refactor surface
pub(crate) use send::SendMaxEstimateResult;
#[allow(unused_imports)] // names reachable via `crate::wallet::sync::*`; pre-refactor surface
pub(crate) use send::ShieldTransparentPcztResult;
#[allow(unused_imports)] // names reachable via `crate::wallet::sync::*`; pre-refactor surface
pub(crate) use send::ShieldTransparentResult;
#[allow(unused_imports)] // names reachable via `crate::wallet::sync::*`; pre-refactor surface
pub(crate) use send::ShieldTransparentStatus;
#[allow(unused_imports)] // names reachable via `crate::wallet::sync::*`; pre-refactor surface
pub(crate) use send::{KeystoneMigrationMessage, KeystoneMigrationSigningRequest};
pub use transactions::{
    decrypt_and_store_transaction, get_next_available_address,
    get_previous_transaction_count_for_address, parse_address_request_kind, set_transaction_status,
    AddressRequestKind,
};
#[allow(unused_imports)] // ditto
pub(crate) use transactions::{
    get_export_birthday_anchor, get_oldest_mined_transaction_anchor, get_transaction_data_requests,
    get_transaction_detail, get_transaction_history, get_unmined_txids_with_mined_output_evidence,
    get_wallet_balance, get_wallet_balances, ExportBirthdayAnchor, TransactionDetail,
    TransactionDetailOutput, TransactionInfo, TxDataRequest, WalletBalance,
    WalletBalanceAvailability,
};

pub(crate) fn open_wallet_db(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_with_timeout(db_path, network, WALLET_DB_BUSY_TIMEOUT)
}

pub(crate) fn open_wallet_db_for_read(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_for_read_with_timeout(db_path, network, READ_DB_BUSY_TIMEOUT)
}

pub(crate) fn open_readonly_conn(db_path: &str) -> Result<rusqlite::Connection, String> {
    open_readonly_conn_with_timeout(db_path, Some(READ_DB_BUSY_TIMEOUT))
}

pub(crate) fn open_readonly_conn_fail_fast(db_path: &str) -> Result<rusqlite::Connection, String> {
    open_readonly_conn_with_timeout(db_path, None)
}

fn open_block_cache(cache_path: &str) -> Result<FsBlockDb, String> {
    std::fs::create_dir_all(cache_path).map_err(|e| format!("Failed to create cache dir: {e}"))?;
    let mut db_cache = FsBlockDb::for_path(cache_path)
        .map_err(|e| format!("Failed to open block cache: {e:?}"))?;
    init_blockmeta_db(&mut db_cache).map_err(|e| format!("Failed to init block cache: {e}"))?;
    Ok(db_cache)
}

// ======================== Sync ========================

pub fn update_chain_tip(db_path: &str, network: WalletNetwork, height: u64) -> Result<(), String> {
    with_wallet_db_write_lock("sync.update_chain_tip", || {
        let mut db = open_wallet_db(db_path, network)?;
        db.update_chain_tip(BlockHeight::from_u32(height as u32))
            .map_err(|e| format!("Failed to update chain tip: {e}"))
    })
}

/// Get next subtree indices to know where to start downloading from.
pub fn get_next_subtree_indices(
    db_path: &str,
    network: WalletNetwork,
) -> Result<(u64, u64, u64), String> {
    let summary = crate::wallet::wallet_summary_cache::get_wallet_summary_cached(db_path, network)?;
    match summary {
        Some(s) => Ok((
            s.next_sapling_subtree_index(),
            s.next_orchard_subtree_index(),
            s.next_ironwood_subtree_index(),
        )),
        None => Ok((0, 0, 0)),
    }
}

pub fn put_sapling_subtree_roots(
    db_path: &str,
    network: WalletNetwork,
    start_index: u64,
    roots: &[(u64, Vec<u8>)],
) -> Result<(), String> {
    let parsed: Vec<_> = roots
        .iter()
        .map(|(h, bytes)| {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "bad hash len")?;
            let node =
                Option::from(sapling_crypto::Node::from_bytes(arr)).ok_or("bad sapling hash")?;
            Ok::<_, String>(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(*h as u32),
                node,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        Ok(())
    } else {
        with_wallet_db_write_lock("sync.put_sapling_subtree_roots", || {
            let mut db = open_wallet_db(db_path, network)?;
            db.put_sapling_subtree_roots(start_index, parsed.as_slice())
                .map_err(|e| format!("{e}"))
        })
    }
}

pub fn put_orchard_subtree_roots(
    db_path: &str,
    network: WalletNetwork,
    start_index: u64,
    roots: &[(u64, Vec<u8>)],
) -> Result<(), String> {
    let parsed: Vec<_> = roots
        .iter()
        .map(|(h, bytes)| {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "bad hash len")?;
            let node = Option::from(orchard::tree::MerkleHashOrchard::from_bytes(&arr))
                .ok_or("bad orchard hash")?;
            Ok::<_, String>(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(*h as u32),
                node,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        Ok(())
    } else {
        with_wallet_db_write_lock("sync.put_orchard_subtree_roots", || {
            let mut db = open_wallet_db(db_path, network)?;
            db.put_orchard_subtree_roots(start_index, parsed.as_slice())
                .map_err(|e| format!("{e}"))
        })
    }
}

pub fn put_ironwood_subtree_roots(
    db_path: &str,
    network: WalletNetwork,
    start_index: u64,
    roots: &[(u64, Vec<u8>)],
) -> Result<(), String> {
    let parsed: Vec<_> = roots
        .iter()
        .map(|(h, bytes)| {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "bad hash len")?;
            let node = Option::from(orchard::tree::MerkleHashOrchard::from_bytes(&arr))
                .ok_or("bad ironwood hash")?;
            Ok::<_, String>(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(*h as u32),
                node,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        Ok(())
    } else {
        with_wallet_db_write_lock("sync.put_ironwood_subtree_roots", || {
            let mut db = open_wallet_db(db_path, network)?;
            db.put_ironwood_subtree_roots(start_index, parsed.as_slice())
                .map_err(|e| format!("{e}"))
        })
    }
}

pub(crate) struct ScanRangeInfo {
    pub start: u64,
    pub end: u64,
    pub priority: u8,
}

pub(crate) fn suggest_scan_ranges(
    db_path: &str,
    network: WalletNetwork,
) -> Result<Vec<ScanRangeInfo>, String> {
    let db = open_wallet_db_for_read(db_path, network)?;
    let ranges = db.suggest_scan_ranges().map_err(|e| format!("{e}"))?;
    Ok(ranges
        .into_iter()
        .filter(|r| r.priority() != ScanPriority::Ignored && r.priority() != ScanPriority::Scanned)
        .map(|r| ScanRangeInfo {
            start: u32::from(r.block_range().start) as u64,
            end: u32::from(r.block_range().end) as u64,
            priority: match r.priority() {
                ScanPriority::Verify => 7,
                ScanPriority::ChainTip => 6,
                ScanPriority::FoundNote => 5,
                ScanPriority::OpenAdjacent => 4,
                ScanPriority::LatestPoolActivation => 3,
                ScanPriority::Historic => 2,
                ScanPriority::Scanned => 1,
                ScanPriority::Ignored => 0,
            },
        })
        .collect())
}

pub fn write_block_metadata(
    cache_path: &str,
    blocks: &[(u64, Vec<u8>, u32, u32, u32)],
) -> Result<(), String> {
    let db_cache = open_block_cache(cache_path)?;
    let metas: Vec<BlockMeta> = blocks
        .iter()
        .map(|(h, hash, time, sc, oc)| {
            let mut arr = [0u8; 32];
            arr[..hash.len().min(32)].copy_from_slice(&hash[..hash.len().min(32)]);
            BlockMeta {
                height: BlockHeight::from_u32(*h as u32),
                block_hash: BlockHash(arr),
                block_time: *time,
                sapling_outputs_count: *sc,
                orchard_actions_count: *oc,
            }
        })
        .collect();
    db_cache
        .write_block_metadata(&metas)
        .map_err(|e| format!("{e:?}"))
}

pub fn scan_blocks(
    db_path: &str,
    cache_path: &str,
    network: WalletNetwork,
    from_height: u64,
    ts_network: &str,
    ts_height: u64,
    ts_hash: &str,
    ts_time: u32,
    ts_sapling: &str,
    ts_orchard: &str,
    ts_ironwood: &str,
    limit: u64,
) -> Result<u64, String> {
    let db_cache = open_block_cache(cache_path)?;
    let from_state = if ts_hash.is_empty() {
        zcash_client_backend::data_api::chain::ChainState::empty(
            BlockHeight::from_u32((from_height - 1) as u32),
            BlockHash([0u8; 32]),
        )
    } else {
        TreeState {
            network: ts_network.into(),
            height: ts_height,
            hash: ts_hash.into(),
            time: ts_time,
            sapling_tree: ts_sapling.into(),
            orchard_tree: ts_orchard.into(),
            ironwood_tree: ts_ironwood.into(),
        }
        .to_chain_state()
        .map_err(|e| format!("{e}"))?
    };
    let result = with_wallet_db_write_lock("sync.scan_blocks", || {
        let mut db_data = open_wallet_db(db_path, network)?;
        scan_cached_blocks(
            &network,
            &db_cache,
            &mut db_data,
            BlockHeight::from_u32(from_height as u32),
            &from_state,
            limit as usize,
        )
        .map_err(|e| format!("{e}"))
    })?;
    Ok((u32::from(result.scanned_range().end) - u32::from(result.scanned_range().start)) as u64)
}

// ======================== Balance & Progress ========================

pub(crate) struct SyncProgress {
    pub scanned_height: u64,
    pub chain_tip_height: u64,
    pub is_syncing: bool,
    pub is_complete: bool,
}

fn is_completed_sync_status(
    scanned_height: u64,
    chain_tip_height: u64,
    last_completed_height: Option<u64>,
) -> bool {
    chain_tip_height > 0
        && scanned_height >= chain_tip_height
        && last_completed_height == Some(chain_tip_height)
}

/// Reads only the two wallet heights needed for status and completion checks.
///
/// `WalletSummary` uses `birthday - 1` before the first block has been fully
/// scanned, so preserve that behavior when `block_fully_scanned` has no value.
pub(crate) fn wallet_scan_heights(db: &mut WalletDatabase) -> Result<Option<(u64, u64)>, String> {
    wallet_scan_heights_in_snapshot(db, || {})
}

fn wallet_scan_heights_in_snapshot(
    db: &mut WalletDatabase,
    after_chain_height: impl FnOnce(),
) -> Result<Option<(u64, u64)>, String> {
    // The callback is a deterministic test seam: the first SELECT has fixed
    // the SQLite snapshot before a concurrent writer is allowed to commit.
    // Production callers always pass a no-op.
    let mut after_chain_height = Some(after_chain_height);
    db.transactionally(|db| {
        let Some(chain_tip_height) = db.chain_height()? else {
            return Ok(None);
        };
        if let Some(after_chain_height) = after_chain_height.take() {
            after_chain_height();
        }
        let scanned_height = match db.block_fully_scanned()? {
            Some(block) => u32::from(block.block_height()) as u64,
            None => {
                let Some(birthday_height) = db.get_wallet_birthday()? else {
                    return Ok(None);
                };
                u32::from(birthday_height).saturating_sub(1) as u64
            }
        };

        Ok(Some((scanned_height, u32::from(chain_tip_height) as u64)))
    })
    .map_err(|e: zcash_client_sqlite::error::SqliteClientError| format!("{e}"))
}

pub(crate) fn get_sync_progress(
    db_path: &str,
    network: WalletNetwork,
) -> Result<SyncProgress, String> {
    let mut db = open_wallet_db_for_read(db_path, network)?;
    match wallet_scan_heights(&mut db)? {
        Some((scanned_height, chain_tip_height)) => {
            let last_completed_height = super::sync_engine::completed_sync_height_for_status(
                db_path,
                scanned_height,
                chain_tip_height,
            )
            .unwrap_or_else(|e| {
                log::warn!("sync: completed-height metadata unavailable: {e}");
                None
            });
            Ok(SyncProgress {
                scanned_height,
                chain_tip_height,
                is_syncing: scanned_height < chain_tip_height,
                is_complete: is_completed_sync_status(
                    scanned_height,
                    chain_tip_height,
                    last_completed_height,
                ),
            })
        }
        None => Ok(SyncProgress {
            scanned_height: 0,
            chain_tip_height: 0,
            is_syncing: false,
            is_complete: false,
        }),
    }
}

// ======================== Rewind ========================

pub fn rewind_to_height(db_path: &str, network: WalletNetwork, height: u64) -> Result<u64, String> {
    let result = with_wallet_db_write_lock("sync.rewind_to_height", || {
        let mut db = open_wallet_db(db_path, network)?;
        db.truncate_to_height(BlockHeight::from_u32(height as u32))
            .map_err(|e| format!("{e}"))
    })?;
    Ok(u32::from(result) as u64)
}

// ======================== Address Validation ========================

pub fn validate_address(address: &str) -> Result<String, String> {
    use zcash_address::ZcashAddress;
    use zcash_keys::address::Address;

    let addr: Address = ZcashAddress::try_from_encoded(address)
        .map_err(|e| format!("Invalid: {e}"))?
        .convert()
        .map_err(|e| format!("Invalid: {e}"))?;

    match addr {
        Address::Unified(_) => Ok("unified".into()),
        Address::Sapling(_) => Ok("sapling".into()),
        Address::Transparent(_) => Ok("transparent".into()),
        Address::Tex(_) => Ok("tex".into()),
    }
}

// ======================== Send ========================

/// Propose a transfer. Returns (proposal_id, needs_sapling_params, fee_zatoshi).
/// The proposal is stored internally and referenced by proposal_id for execute_proposal.
// In-memory proposal store (proposals are short-lived, between
// propose and execute). Kept in `sync/mod.rs` because it is shared
// between the software send flow (`send::execute_proposal`) and the
// hardware PCZT pipeline (`pczt::create_pczt_from_proposal`); placing
// it in either submodule would create a cross-submodule dependency.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(super) struct StoredProposal {
    pub proposal_id: u64,
    pub proposal: zcash_client_backend::proposal::Proposal<
        send::WalletFeeRule,
        zcash_client_sqlite::ReceivedNoteId,
    >,
    pub proposed_tx_version: Option<zcash_primitives::transaction::TxVersion>,
    pub network: WalletNetwork,
    pub account_id: AccountUuid,
    pub send_flow_id: String,
}

#[derive(Clone)]
pub(super) struct StoredProposalLock {
    pub proposal: zcash_client_backend::proposal::Proposal<
        send::WalletFeeRule,
        zcash_client_sqlite::ReceivedNoteId,
    >,
    pub network: WalletNetwork,
    pub db_path: String,
    pub owner: zcash_client_backend::wallet::LockOwner,
    pub send_flow_id: String,
}

pub(super) static PROPOSAL_STORE: std::sync::LazyLock<Mutex<ProposalStore>> =
    std::sync::LazyLock::new(|| {
        Mutex::new(ProposalStore {
            proposals: HashMap::new(),
            locks: HashMap::new(),
            names_transactions: HashMap::new(),
            next_id: 1,
        })
    });

pub(super) struct ProposalStore {
    pub proposals: HashMap<u64, StoredProposal>,
    pub locks: HashMap<u64, StoredProposalLock>,
    /// Prebuilt Names transactions have a distinct map from ordinary send
    /// proposals. Both maps draw IDs from `next_id` while sharing this mutex,
    /// so a stale ordinary-send operation can never consume a Names
    /// capability (or vice versa).
    pub names_transactions: HashMap<u64, NamesTransactionCapability>,
    pub next_id: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NamesTransactionExecution {
    pub raw: Vec<u8>,
    pub txid: [u8; 32],
    pub db_path: String,
    pub network: WalletNetwork,
    pub account_uuid: String,
    pub name: String,
    pub valid_from_height: u32,
    pub expiry_height: u32,
    pub fee_zatoshi: u64,
    pub kind: NamesTransactionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NamesTransactionKind {
    Reveal,
    Update,
    Renew,
    Release,
}

impl NamesTransactionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Reveal => "REVEAL",
            Self::Update => "UPDATE",
            Self::Renew => "RENEW",
            Self::Release => "RELEASE",
        }
    }

    fn activity_action(self) -> &'static str {
        match self {
            Self::Reveal => "reveal",
            Self::Update => "update",
            Self::Renew => "renew",
            Self::Release => "release",
        }
    }
}

/// Minimal bounded metadata that must survive a retain operation. The store
/// does not interpret the payload or the eventual Names transaction here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NamesTransactionLockMetadata {
    pub expiry_height: u64,
}

/// Capability-specific release work is supplied as Send + Sync data. The
/// `Arc` makes the record movable out of the store mutex; removing the record
/// before invoking the callback makes release at-most-once even on failure.
pub(super) type NamesTransactionRelease =
    Arc<dyn Fn() -> Result<(), String> + Send + Sync + 'static>;
pub(super) type NamesTransactionRetain =
    Arc<dyn Fn(NamesTransactionLockMetadata) -> Result<(), String> + Send + Sync + 'static>;

/// Cleanup actions are intentionally generic and contain only Send + Sync
/// callback data. The final Names transaction type is not part of stage 1.
pub(super) enum NamesTransactionCleanup {
    Callbacks {
        release: NamesTransactionRelease,
        retain: NamesTransactionRetain,
    },
}

pub(super) struct NamesTransactionCapability {
    pub proposal_id: u64,
    pub send_flow_id: String,
    pub execution: NamesTransactionExecution,
    pub lock: NamesTransactionLockMetadata,
    pub cleanup: NamesTransactionCleanup,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NamesTransactionCapabilityView {
    pub proposal_id: u64,
    pub send_flow_id: String,
    pub execution: NamesTransactionExecution,
    pub lock: NamesTransactionLockMetadata,
}

impl NamesTransactionCapability {
    #[allow(dead_code)]
    fn view(&self) -> NamesTransactionCapabilityView {
        NamesTransactionCapabilityView {
            proposal_id: self.proposal_id,
            send_flow_id: self.send_flow_id.clone(),
            execution: self.execution.clone(),
            lock: self.lock.clone(),
        }
    }

    /// Finish a consumed capability's release path. Taking ownership here
    /// prevents callers from accidentally invoking cleanup more than once.
    pub(super) fn release(self) -> Result<(), String> {
        let NamesTransactionCleanup::Callbacks { release, .. } = self.cleanup;
        release()
    }

    /// Finish a consumed capability's retain path, preserving the bounded
    /// lock metadata in the capability-specific callback.
    pub(super) fn retain(self) -> Result<(), String> {
        let NamesTransactionCleanup::Callbacks { retain, .. } = self.cleanup;
        retain(self.lock)
    }
}

/// Allocate an ID from the same counter used by ordinary send proposals.
/// Keeping this helper next to the store prevents a future capability map
/// from accidentally introducing an independent ID allocator.
pub(super) fn next_proposal_id(store: &mut ProposalStore) -> u64 {
    let id = store.next_id;
    store.next_id += 1;
    id
}

/// Store an opaque Names transaction capability and its bounded lock metadata.
pub(super) fn allocate_names_transaction_capability(
    send_flow_id: &str,
    execution: NamesTransactionExecution,
    lock: NamesTransactionLockMetadata,
    cleanup: NamesTransactionCleanup,
) -> Result<u64, String> {
    if send_flow_id.is_empty() {
        return Err("Names send flow ID cannot be empty".to_string());
    }
    let mut store = PROPOSAL_STORE
        .lock()
        .map_err(|e| format!("Lock proposal store for Names allocation: {e}"))?;
    let proposal_id = next_proposal_id(&mut store);
    store.names_transactions.insert(
        proposal_id,
        NamesTransactionCapability {
            proposal_id,
            send_flow_id: send_flow_id.to_string(),
            execution,
            lock,
            cleanup,
        },
    );
    Ok(proposal_id)
}

fn names_transaction_flow_matches(
    capability: &NamesTransactionCapability,
    send_flow_id: &str,
) -> Result<(), String> {
    if capability.send_flow_id == send_flow_id {
        Ok(())
    } else {
        log::warn!(
            "proposal store: Names send flow mismatch for proposal_id={}",
            capability.proposal_id
        );
        Err("Send flow mismatch".to_string())
    }
}

/// Inspect without consuming. A flow mismatch is distinguished from an
/// absent capability so callers cannot probe another flow's payload.
#[allow(dead_code)]
pub(super) fn inspect_names_transaction_capability(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<Option<NamesTransactionCapabilityView>, String> {
    let store = PROPOSAL_STORE
        .lock()
        .map_err(|e| format!("Lock proposal store to inspect Names transaction: {e}"))?;
    let Some(capability) = store.names_transactions.get(&proposal_id) else {
        return Ok(None);
    };
    names_transaction_flow_matches(capability, send_flow_id)?;
    Ok(Some(capability.view()))
}

/// Consume a Names transaction capability exactly once.
#[allow(dead_code)]
pub(super) fn take_names_transaction_capability(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<NamesTransactionCapability, String> {
    try_take_names_transaction_capability(proposal_id, send_flow_id)?.ok_or_else(|| {
        "Names transaction capability not found (expired or already consumed)".to_string()
    })
}

/// Atomically inspect and consume a Names transaction capability. `None` means no
/// Names capability exists for this ID; a present capability with another
/// flow ID is still rejected without consuming it.
pub(super) fn try_take_names_transaction_capability(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<Option<NamesTransactionCapability>, String> {
    let mut store = PROPOSAL_STORE
        .lock()
        .map_err(|e| format!("Lock proposal store to consume Names transaction: {e}"))?;
    let Some(capability) = store.names_transactions.get(&proposal_id) else {
        return Ok(None);
    };
    names_transaction_flow_matches(capability, send_flow_id)?;
    Ok(store.names_transactions.remove(&proposal_id))
}

/// Discard a Names transaction capability. Removing it before release work makes
/// this idempotent and guarantees that a release callback is invoked once.
/// The boolean reports whether this was a Names capability, allowing generic
/// proposal APIs to fall through to their unchanged ordinary-send logic.
pub(super) fn discard_names_transaction_capability(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<bool, String> {
    let capability = {
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store for Names discard: {e}"))?;
        let Some(capability) = store.names_transactions.get(&proposal_id) else {
            return Ok(false);
        };
        names_transaction_flow_matches(capability, send_flow_id)?;
        store.names_transactions.remove(&proposal_id)
    };
    let capability =
        capability.ok_or_else(|| "Names capability disappeared while discarding".to_string())?;
    capability.release()?;
    Ok(true)
}

/// Remove replay capability while handing its bounded lock metadata to the
/// capability-specific retain callback. Removing before callback execution
/// makes a repeated retain a no-op and guarantees one callback invocation.
pub(super) fn retain_names_transaction_capability_until_expiry(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<bool, String> {
    let capability = {
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store for Names retain: {e}"))?;
        let Some(capability) = store.names_transactions.get(&proposal_id) else {
            return Ok(false);
        };
        names_transaction_flow_matches(capability, send_flow_id)?;
        store.names_transactions.remove(&proposal_id)
    };
    let capability =
        capability.ok_or_else(|| "Names capability disappeared while retaining".to_string())?;
    capability.retain()?;
    Ok(true)
}

/// Try the reviewed Names transaction path before ordinary mnemonic
/// handling. The capability is consumed atomically before any seed handling.
pub(crate) async fn try_execute_names_transaction_proposal(
    db_path: &str,
    lightwalletd_url: &str,
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<Option<ExecuteProposalResult>, String> {
    let Some(capability) = try_take_names_transaction_capability(proposal_id, send_flow_id)? else {
        return Ok(None);
    };
    let execution = capability.execution.clone();

    if db_path != execution.db_path {
        let error = "Names proposal belongs to a different wallet database".to_string();
        return match capability.release() {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; additionally failed to release the Names fee note: {cleanup_error}"
            )),
        };
    }

    if let Err(error) = crate::wallet::names_lifecycle::ensure_transaction_window_open(
        lightwalletd_url,
        execution.valid_from_height,
        execution.expiry_height,
    )
    .await
    {
        return match capability.release() {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; additionally failed to release the Names fee note: {cleanup_error}"
            )),
        };
    }

    match broadcast_raw_transaction_isolated(lightwalletd_url, &execution.raw).await {
        Ok(()) => {
            let mut warnings = Vec::new();
            if let Err(error) = capability.retain() {
                warnings.push(format!(
                    "failed to retain the Names {} fee lock after broadcast: {error}",
                    execution.kind.label()
                ));
            }
            if let Err(error) = decrypt_and_store_transaction(
                &execution.db_path,
                execution.network,
                &execution.raw,
                None,
            ) {
                warnings.push(format!(
                    "broadcast succeeded but local transaction storage failed: {error}"
                ));
            } else {
                let metadata_result = if execution.kind == NamesTransactionKind::Reveal {
                    crate::wallet::coppice::record_reveal_broadcast(
                        &execution.db_path,
                        &execution.account_uuid,
                        &execution.name,
                        execution.txid,
                    )
                } else {
                    crate::wallet::coppice::record_names_activity(
                        &execution.db_path,
                        &execution.account_uuid,
                        &execution.name,
                        execution.kind.activity_action(),
                        execution.txid,
                    )
                };
                if let Err(error) = metadata_result {
                    warnings.push(format!(
                        "broadcast and local transaction storage succeeded but Names metadata update failed: {error}"
                    ));
                }
            }
            let message = if warnings.is_empty() {
                None
            } else {
                for warning in &warnings {
                    log::warn!("Names {}: {warning}", execution.kind.label());
                }
                Some(warnings.join("; "))
            };
            Ok(Some(ExecuteProposalResult {
                txids: hex::encode(execution.txid),
                // The node accepted the transaction; local persistence
                // warnings must not turn this into an apparent unbroadcast
                // failure that a caller retries.
                status: "broadcasted".to_string(),
                broadcasted_count: 1,
                total_count: 1,
                message,
            }))
        }
        Err(error) if error.starts_with("Broadcast rejected:") => {
            capability.release()?;
            Err(error)
        }
        Err(error) => {
            let retain_error = capability.retain().err();
            let message = match retain_error {
                Some(retain_error) => format!(
                    "broadcast outcome is unknown ({error}); failed to retain bounded Names fee lock: {retain_error}"
                ),
                None => format!("broadcast outcome is unknown: {error}"),
            };
            log::warn!("Names {}: {message}", execution.kind.label());
            Ok(Some(ExecuteProposalResult {
                txids: hex::encode(execution.txid),
                status: "broadcast_unknown".to_string(),
                broadcasted_count: 0,
                total_count: 1,
                message: Some(message),
            }))
        }
    }
}

pub(super) fn consume_stored_proposal(
    proposal_id: u64,
    send_flow_id: &str,
    not_found_message: &str,
) -> Result<StoredProposal, String> {
    let mut store = PROPOSAL_STORE
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;

    match store.proposals.get(&proposal_id) {
        Some(stored) if stored.send_flow_id == send_flow_id => {}
        Some(_) => {
            log::warn!("proposal store: send flow mismatch for proposal_id={proposal_id}");
            return Err("Send flow mismatch".to_string());
        }
        None => return Err(not_found_message.to_string()),
    }

    store
        .proposals
        .remove(&proposal_id)
        .ok_or_else(|| not_found_message.to_string())
}

pub(super) fn stored_proposal_lock(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<StoredProposalLock, String> {
    let store = PROPOSAL_STORE
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    match store.locks.get(&proposal_id) {
        Some(lock) if lock.send_flow_id == send_flow_id => Ok(lock.clone()),
        Some(_) => {
            log::warn!("proposal store: lock send flow mismatch for proposal_id={proposal_id}");
            Err("Send flow mismatch".to_string())
        }
        None => Err("Proposal input lock not found".to_string()),
    }
}

fn unlock_stored_proposal(
    proposal_id: u64,
    send_flow_id: &str,
    lock: StoredProposalLock,
) -> Result<(), String> {
    with_wallet_db_write_lock("sync.unlock_stored_proposal", || {
        let mut db = open_wallet_db(&lock.db_path, lock.network)?;
        // The wallet write lock is always acquired before the proposal-store
        // mutex (the same order used while creating proposals). Re-checking
        // here prevents a retain call that won the race from being followed by
        // a stale DB unlock.
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store before DB unlock: {e}"))?;
        let current = match store.locks.get(&proposal_id) {
            Some(current) if current.send_flow_id == send_flow_id => current.clone(),
            Some(_) => return Err("Send flow mismatch before DB unlock".to_string()),
            None => return Ok(()),
        };
        zcash_client_backend::data_api::wallet::unlock_proposal_inputs(
            &mut db,
            &current.proposal,
            current.owner,
        )
        .map_err(|e| format!("Unlock abandoned send proposal inputs: {e}"))?;
        proposal_locks::remove(&current.db_path, current.owner)?;
        store.locks.remove(&proposal_id);
        Ok(())
    })
}

pub(super) fn finish_stored_proposal(
    proposal_id: u64,
    send_flow_id: &str,
    release_inputs: bool,
) -> Result<(), String> {
    let lock = {
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store for finish: {e}"))?;
        let Some(lock) = store.locks.get(&proposal_id).cloned() else {
            return Ok(());
        };
        if lock.send_flow_id != send_flow_id {
            return Err("Send flow mismatch".to_string());
        }
        if !release_inputs {
            store.locks.remove(&proposal_id);
            drop(store);
            return proposal_locks::remove(&lock.db_path, lock.owner);
        }
        lock.clone()
    };

    // The DB helper re-checks ownership while holding both locks. On DB
    // failure the owner record remains in place, allowing an idempotent retry.
    unlock_stored_proposal(proposal_id, send_flow_id, lock)
}

pub(super) fn discard_stored_proposal(proposal_id: u64, send_flow_id: &str) -> Result<(), String> {
    let should_release = {
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store for discard: {e}"))?;
        match store.proposals.get(&proposal_id) {
            Some(stored) if stored.send_flow_id == send_flow_id => {
                store.proposals.remove(&proposal_id);
                true
            }
            Some(_) => return Err("Send flow mismatch".to_string()),
            None => match store.locks.get(&proposal_id) {
                Some(lock) if lock.send_flow_id == send_flow_id => true,
                Some(_) => return Err("Send flow mismatch".to_string()),
                None => false,
            },
        }
    };
    if should_release {
        finish_stored_proposal(proposal_id, send_flow_id, true)?;
    }
    Ok(())
}

/// Removes all in-memory capability to reuse or explicitly unlock a proposal,
/// while leaving its wallet-level input lock to expire at its original height.
///
/// This is used when a broadcast may have reached the network but local
/// transaction storage did not complete. Releasing the DB lock in that state
/// could allow an immediate conflicting send.
pub(super) fn retain_stored_proposal_lock_until_expiry(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<(), String> {
    let lock = {
        let store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store to inspect retained DB lock: {e}"))?;
        match store.locks.get(&proposal_id) {
            Some(lock) if lock.send_flow_id == send_flow_id => Some(lock.clone()),
            Some(_) => return Err("Send flow mismatch".to_string()),
            None => None,
        }
    };
    let Some(lock) = lock else {
        return Ok(());
    };

    with_wallet_db_write_lock("sync.retain_stored_proposal_lock", || {
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store to retain DB lock: {e}"))?;
        let Some(current) = store.locks.get(&proposal_id) else {
            return Ok(());
        };
        if current.send_flow_id != send_flow_id || current.owner != lock.owner {
            return Err("Send flow changed before retaining DB lock".to_string());
        }
        proposal_locks::mark_retain_until_expiry(&lock.db_path, lock.owner)?;
        if let Some(proposal) = store.proposals.get(&proposal_id) {
            if proposal.send_flow_id != send_flow_id {
                return Err("Send flow mismatch".to_string());
            }
            store.proposals.remove(&proposal_id);
        }
        // Remove the unlock capability in the same critical section as the
        // replayable proposal. A concurrent discard can therefore observe
        // either the complete pre-retain state or the complete retained state,
        // never the gap between them.
        store.locks.remove(&proposal_id);
        Ok(())
    })
}

// ======================== Helpers ========================

pub fn get_blocks_dir(cache_path: &str) -> String {
    format!("{cache_path}/blocks")
}

#[cfg(test)]
mod tests {
    //! Regression tests for PROPOSAL_STORE lifecycle.
    //!
    //! These tests cover the parts of the proposal store that don't require a
    //! real wallet DB (note selection, fee computation, etc. are upstream of
    //! anything testable in isolation). Specifically:
    //!
    //! - `discard_proposal` is idempotent and tolerates nonexistent IDs
    //!   (called from the Dart cancel path and possibly more than once).
    //! - `create_pczt_from_proposal` returns a clean "not found" error for
    //!   an unknown ID instead of panicking or corrupting state — this is
    //!   the path that fires on a replay attempt after the proposal has
    //!   already been consumed.
    //!
    //! A full insert→consume→replay test would require constructing a real
    //! `Proposal<WalletFeeRule, ReceivedNoteId>`, which in turn needs a
    //! live wallet DB with spendable notes and a lightwalletd chain tip.
    //! That belongs in an integration test, not a unit test here.

    use super::*;

    #[test]
    fn sync_completion_requires_matching_persisted_tip() {
        assert!(is_completed_sync_status(100, 100, Some(100)));
        assert!(!is_completed_sync_status(100, 100, None));
        assert!(!is_completed_sync_status(100, 100, Some(99)));
        assert!(!is_completed_sync_status(99, 100, Some(100)));
        assert!(!is_completed_sync_status(0, 0, Some(0)));
    }

    #[test]
    fn sync_progress_preserves_pre_scan_birthday_height_without_wallet_summary() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path = db_path.to_str().unwrap();
        let phrase = crate::wallet::keys::generate_mnemonic();
        let seed = crate::wallet::keys::mnemonic_to_seed(&phrase).unwrap();

        crate::wallet::keys::init_db_and_create_account(
            db_path,
            WalletNetwork::Regtest,
            &seed,
            Some(1_000),
            "test",
        )
        .unwrap();
        update_chain_tip(db_path, WalletNetwork::Regtest, 1_100).unwrap();

        let progress = get_sync_progress(db_path, WalletNetwork::Regtest).unwrap();
        assert_eq!(progress.scanned_height, 999);
        assert_eq!(progress.chain_tip_height, 1_100);
        assert!(progress.is_syncing);
        assert!(!progress.is_complete);
    }

    #[test]
    fn wallet_scan_heights_uses_one_sqlite_snapshot() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path = db_path.to_str().unwrap();
        let phrase = crate::wallet::keys::generate_mnemonic();
        let seed = crate::wallet::keys::mnemonic_to_seed(&phrase).unwrap();

        crate::wallet::keys::init_db_and_create_account(
            db_path,
            WalletNetwork::Regtest,
            &seed,
            Some(1_000),
            "test",
        )
        .unwrap();
        update_chain_tip(db_path, WalletNetwork::Regtest, 1_100).unwrap();

        let mut db = open_wallet_db_for_read(db_path, WalletNetwork::Regtest).unwrap();
        let heights = wallet_scan_heights_in_snapshot(&mut db, || {
            let writer = rusqlite::Connection::open(db_path).unwrap();
            writer
                .execute("UPDATE accounts SET birthday_height = 500", [])
                .unwrap();
        })
        .unwrap();

        assert_eq!(heights, Some((999, 1_100)));
        let updated_birthday: u32 = rusqlite::Connection::open(db_path)
            .unwrap()
            .query_row("SELECT MIN(birthday_height) FROM accounts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(updated_birthday, 500);
    }

    /// Pull a proposal ID that is guaranteed not to collide with anything a
    /// concurrent test might have inserted. We use a fresh counter so each
    /// call yields a distinct u64.
    fn unique_proposal_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        // Start well above next_id's initial value (1) to avoid any overlap
        // with proposals that a parallel test might genuinely insert.
        static COUNTER: AtomicU64 = AtomicU64::new(1_000_000_000);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    fn names_capability(flow: &str) -> u64 {
        allocate_names_transaction_capability(
            flow,
            NamesTransactionExecution {
                raw: vec![0x42],
                txid: [0; 32],
                db_path: "test.db".to_string(),
                network: WalletNetwork::Regtest,
                account_uuid: "test-account".to_string(),
                name: "test.zec".to_string(),
                valid_from_height: 123,
                expiry_height: 123,
                fee_zatoshi: 1,
                kind: NamesTransactionKind::Reveal,
            },
            NamesTransactionLockMetadata { expiry_height: 123 },
            NamesTransactionCleanup::Callbacks {
                release: Arc::new(|| Ok(())),
                retain: Arc::new(|_| Ok(())),
            },
        )
        .unwrap()
    }

    fn remove_names_test_entries(ids: &[u64]) {
        let mut store = PROPOSAL_STORE.lock().unwrap();
        for id in ids {
            store.names_transactions.remove(id);
        }
    }

    #[test]
    fn names_reveal_ids_use_shared_noncolliding_namespace() {
        let first = names_capability("names-id-flow-1");
        let second = names_capability("names-id-flow-2");
        assert_ne!(first, second);
        let store = PROPOSAL_STORE.lock().unwrap();
        assert!(store.names_transactions.contains_key(&first));
        assert!(store.names_transactions.contains_key(&second));
        assert!(store.next_id > second);
        assert!(!store.proposals.contains_key(&first));
        assert!(!store.locks.contains_key(&first));
        drop(store);
        remove_names_test_entries(&[first, second]);
    }

    #[test]
    fn names_reveal_inspect_and_take_reject_mismatch_and_consume_once() {
        let id = names_capability("names-take-flow");
        let mismatch = inspect_names_transaction_capability(id, "wrong-flow");
        assert!(matches!(mismatch, Err(ref error) if error == "Send flow mismatch"));

        let inspected = inspect_names_transaction_capability(id, "names-take-flow")
            .unwrap()
            .unwrap();
        assert_eq!(inspected.execution.raw, vec![0x42]);
        let taken = take_names_transaction_capability(id, "names-take-flow").unwrap();
        assert_eq!(taken.execution.raw, vec![0x42]);
        assert!(take_names_transaction_capability(id, "names-take-flow").is_err());
        remove_names_test_entries(&[id]);
    }

    #[test]
    fn generic_discard_names_reveal_is_idempotent_and_releases_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let release_count = Arc::new(AtomicUsize::new(0));
        let release_count_for_callback = release_count.clone();
        let id = allocate_names_transaction_capability(
            "names-discard-flow",
            NamesTransactionExecution {
                raw: vec![1, 2, 3],
                txid: [0; 32],
                db_path: "test.db".to_string(),
                network: WalletNetwork::Regtest,
                account_uuid: "test-account".to_string(),
                name: "test.zec".to_string(),
                valid_from_height: 123,
                expiry_height: 456,
                fee_zatoshi: 1,
                kind: NamesTransactionKind::Release,
            },
            NamesTransactionLockMetadata { expiry_height: 456 },
            NamesTransactionCleanup::Callbacks {
                release: Arc::new(move || {
                    release_count_for_callback.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                retain: Arc::new(|_| Ok(())),
            },
        )
        .unwrap();

        assert!(pczt::discard_proposal(id, "names-discard-flow").is_ok());
        assert!(pczt::discard_proposal(id, "names-discard-flow").is_ok());
        assert_eq!(release_count.load(Ordering::SeqCst), 1);
        remove_names_test_entries(&[id]);
    }

    #[test]
    fn generic_retain_removes_replay_capability_once_and_preserves_lock_metadata() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        let retain_count = Arc::new(AtomicUsize::new(0));
        let retained_height = Arc::new(AtomicU64::new(0));
        let retain_count_for_callback = retain_count.clone();
        let retained_height_for_callback = retained_height.clone();
        let id = allocate_names_transaction_capability(
            "names-retain-flow",
            NamesTransactionExecution {
                raw: vec![0x42],
                txid: [0; 32],
                db_path: "test.db".to_string(),
                network: WalletNetwork::Regtest,
                account_uuid: "test-account".to_string(),
                name: "test.zec".to_string(),
                valid_from_height: 123,
                expiry_height: 123,
                fee_zatoshi: 1,
                kind: NamesTransactionKind::Renew,
            },
            NamesTransactionLockMetadata { expiry_height: 123 },
            NamesTransactionCleanup::Callbacks {
                release: Arc::new(|| Ok(())),
                retain: Arc::new(move |lock| {
                    retain_count_for_callback.fetch_add(1, Ordering::SeqCst);
                    retained_height_for_callback.store(lock.expiry_height, Ordering::SeqCst);
                    Ok(())
                }),
            },
        )
        .unwrap();
        assert!(pczt::retain_proposal_lock_until_expiry(id, "wrong-flow").is_err());
        assert!(
            inspect_names_transaction_capability(id, "names-retain-flow")
                .unwrap()
                .is_some()
        );
        assert!(pczt::retain_proposal_lock_until_expiry(id, "names-retain-flow").is_ok());
        assert!(pczt::retain_proposal_lock_until_expiry(id, "names-retain-flow").is_ok());

        let store = PROPOSAL_STORE.lock().unwrap();
        assert!(!store.names_transactions.contains_key(&id));
        drop(store);
        assert_eq!(retain_count.load(Ordering::SeqCst), 1);
        assert_eq!(retained_height.load(Ordering::SeqCst), 123);
        // A retained capability is already safely non-replayable; generic
        // discard must not run its release callback or recreate the lock.
        assert!(pczt::discard_proposal(id, "names-retain-flow").is_ok());
        remove_names_test_entries(&[id]);
    }

    #[tokio::test]
    async fn names_reveal_execution_dispatch_ignores_unknown_id_without_network_or_seed() {
        let result = try_execute_names_transaction_proposal(
            "/unused/wallet.db",
            "https://unused.invalid",
            unique_proposal_id(),
            "missing-flow",
        )
        .await
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn validate_address_classifies_tex_addresses() {
        assert_eq!(
            validate_address("tex1s2rt77ggv6q989lr49rkgzmh5slsksa9khdgte").unwrap(),
            "tex"
        );
        assert_eq!(
            validate_address("textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy").unwrap(),
            "tex"
        );
    }

    #[test]
    fn validate_address_rejects_invalid_address() {
        assert!(validate_address("not-an-address").is_err());
    }

    #[test]
    fn validate_address_rejects_sprout_addresses() {
        use zcash_address::ToAddress;

        let sprout = zcash_address::ZcashAddress::from_sprout(
            zcash_protocol::consensus::NetworkType::Main,
            [0; 64],
        )
        .to_string();

        assert!(validate_address(&sprout).is_err());
    }

    #[test]
    fn discard_proposal_is_idempotent_for_missing_id() {
        // Should not panic, should not poison the mutex.
        let id = unique_proposal_id();
        discard_proposal(id, "missing-flow").unwrap();
        discard_proposal(id, "missing-flow").unwrap(); // second call must also be a no-op
    }

    #[tokio::test]
    async fn create_pczt_from_proposal_errors_for_missing_id() {
        // A replay attempt (or a bogus ID from stale UI state) must surface
        // a clean "not found" error rather than panicking or creating a
        // bogus PCZT. We pass an invalid db_path because the "not found"
        // check fires before any DB work; if the behavior regresses to
        // touching the DB first, this test will reveal it via a different
        // error message.
        let id = unique_proposal_id();
        let result = create_pczt_from_proposal(
            "/nonexistent/path/that/should/not/exist.db",
            "https://unused.invalid",
            WalletNetwork::Main,
            id,
            "missing-flow",
        )
        .await;

        match result {
            Err(msg) => {
                assert!(
                    msg.contains("Proposal not found"),
                    "expected 'Proposal not found' error, got: {msg}"
                );
            }
            Ok(_) => panic!("create_pczt_from_proposal succeeded for unknown id {id}"),
        }
    }

    #[tokio::test]
    async fn discard_proposal_after_create_pczt_failure_is_still_noop() {
        // Simulates the Dart `finally` cleanup path: after create_pczt
        // fails with "not found" (so the proposal was never there), the
        // finally block still calls discard_proposal. That call must be
        // safe even though the ID has never been in the store.
        let id = unique_proposal_id();
        let _ = create_pczt_from_proposal(
            "/nonexistent/path/that/should/not/exist.db",
            "https://unused.invalid",
            WalletNetwork::Main,
            id,
            "missing-flow",
        )
        .await;
        discard_proposal(id, "missing-flow").unwrap(); // cleanup must not panic
    }
}
