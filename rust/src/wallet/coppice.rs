//! Wallet-owned Coppice/Names host integration.
//!
//! The wallet owns configuration, canonical block acquisition, and durable
//! checkpoints.  It does not validate Names transition policy itself: compact
//! blocks and selectively acquired full transactions are handed to Coppice
//! Core, while exact-name lookups use the Names application's bounded
//! `FreshResolver` over the same canonical source shape.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use coppice::runtime::ApplicationMessageStatus;
use coppice::{
    application::{ApplicationKey, ApplicationSnapshot, PersistedCoppiceApplication},
    compositor::CoppiceRuntime,
    identity::{CoreRuntimeParameters, ValidatedCoreRuntimeParameters, ZcashNetwork},
    replay::{CoreReplay, CoreReplayActivationCheckpoint, CoreReplayConfiguration},
};
use coppice_librustzcash::CanonicalRuntime;
use coppice_librustzcash::FullTransactionSource;
use coppice_names::v1::{
    decode_operations, names_application_id, CanonicalBlock, CanonicalSource, CanonicalTransaction,
    ChainTip, IronwoodActionRef, NamesApplication, OrchardV1ProofVerifier, PaymentNetwork,
    PaymentRecord, ResolutionResult, ResolutionStatus, ResolveError, V1Operation, V1Parameters,
    NAMES_APPLICATION_VERSION,
};
use orchard::note_encryption::CompactAction;
use serde::{Deserialize, Serialize};
use zcash_client_backend::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{compact_tx_streamer_client::CompactTxStreamerClient, RawTransaction, TreeState},
};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::{BlockHeight, BranchId};

use super::{network::WalletNetwork, sync_engine};

const STORE_FORMAT_VERSION: u32 = 1;
const STORE_SUFFIX: &str = ".coppice-names-v1";
const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ACQUIRED_FULL_TRANSACTION_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESOLUTION_BLOCKS: u32 = 100_000;
const ACQUISITION_BATCH_BLOCKS: u32 = 2_000;

/// A validated, explicitly configured Names/Coppice deployment.
///
/// No public deployment values are embedded here.  A wallet integration must
/// receive the runtime identity and rendezvous material from its deployment
/// configuration, which prevents a test vector from silently becoming a
/// production authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NamesWalletConfig {
    network_code: u8,
    runtime_activation_height: u32,
    names: V1Parameters,
    retention_blocks: u32,
    network_domain: Vec<u8>,
    rendezvous_ivk: Vec<u8>,
    rendezvous_receiver: Vec<u8>,
}

impl NamesWalletConfig {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_api(
        network: WalletNetwork,
        runtime_activation_height: u32,
        names_activation_height: u32,
        epoch_size: u32,
        commit_ttl_blocks: u32,
        refresh_deadline_blocks: u32,
        lease_duration_blocks: u32,
        grace_period_blocks: u32,
        reuse_delay_blocks: u32,
        max_record_bytes: usize,
        minimum_bond_zatoshis: u64,
        retention_blocks: u32,
        network_domain: String,
        rendezvous_ivk_hex: String,
        rendezvous_receiver_hex: String,
    ) -> Result<Self, String> {
        if runtime_activation_height == 0 {
            return Err("Coppice runtime activation height must be nonzero".into());
        }
        if names_activation_height < runtime_activation_height {
            return Err("Names activation cannot precede Coppice runtime activation".into());
        }
        if retention_blocks == 0 {
            return Err("Names rewind retention must be nonzero".into());
        }
        if network_domain.is_empty() || network_domain.len() > u16::MAX as usize {
            return Err("Coppice network domain must be 1..65535 bytes".into());
        }
        if max_record_bytes > coppice_names::v1::state::MAX_RECORD_BYTES {
            return Err(format!(
                "Names max record size exceeds {} bytes",
                coppice_names::v1::state::MAX_RECORD_BYTES
            ));
        }
        let names = V1Parameters {
            activation_height: names_activation_height,
            epoch_size,
            commit_ttl_blocks,
            refresh_deadline_blocks,
            lease_duration_blocks,
            grace_period_blocks,
            reuse_delay_blocks,
            max_record_bytes,
            minimum_bond_zatoshis,
        };
        names
            .validate()
            .map_err(|error| format!("invalid Names parameters: {error:?}"))?;
        let rendezvous_ivk = decode_fixed::<64>(&rendezvous_ivk_hex, "rendezvous IVK")?;
        let rendezvous_receiver =
            decode_fixed::<43>(&rendezvous_receiver_hex, "rendezvous receiver")?;
        let config = Self {
            network_code: network_code(network),
            runtime_activation_height,
            names,
            retention_blocks,
            network_domain: network_domain.into_bytes(),
            rendezvous_ivk: rendezvous_ivk.to_vec(),
            rendezvous_receiver: rendezvous_receiver.to_vec(),
        };
        config.validated_core_parameters(network)?;
        Ok(config)
    }

    fn validated_core_parameters(
        &self,
        network: WalletNetwork,
    ) -> Result<ValidatedCoreRuntimeParameters, String> {
        if self.network_code != network_code(network) {
            return Err("Names configuration belongs to a different wallet network".into());
        }
        CoreRuntimeParameters {
            runtime_protocol_id: b"coppice.runtime".to_vec(),
            runtime_protocol_version: 1,
            zcash_network_domain: self.network_domain.clone(),
            zcash_network: core_network(network),
            runtime_activation_height: self.runtime_activation_height,
            carrier_protocol_id: b"CPV1".to_vec(),
            rendezvous_ivk: self
                .rendezvous_ivk
                .clone()
                .try_into()
                .map_err(|bytes: Vec<u8>| {
                    format!(
                        "rendezvous IVK must be exactly 64 bytes, got {}",
                        bytes.len()
                    )
                })?,
            rendezvous_receiver: self.rendezvous_receiver.clone().try_into().map_err(
                |bytes: Vec<u8>| {
                    format!(
                        "rendezvous receiver must be exactly 43 bytes, got {}",
                        bytes.len()
                    )
                },
            )?,
        }
        .validate()
        .map_err(|error| format!("invalid Coppice runtime parameters: {error:?}"))
    }

    fn payment_network(&self) -> PaymentNetwork {
        match self.network_code {
            0 => PaymentNetwork::Main,
            1 => PaymentNetwork::Test,
            _ => PaymentNetwork::Regtest,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredApplicationSnapshot {
    format_version: u32,
    descriptor_id: [u8; 32],
    descriptor_version: u16,
    descriptor_activation_height: u32,
    tip_height: u32,
    tip_block_hash: [u8; 32],
    state_root: [u8; 32],
    oldest_rewind_height: u32,
    payload: Vec<u8>,
}

impl From<ApplicationSnapshot> for StoredApplicationSnapshot {
    fn from(snapshot: ApplicationSnapshot) -> Self {
        Self {
            format_version: snapshot.format_version,
            descriptor_id: snapshot.descriptor.key.id.to_bytes(),
            descriptor_version: snapshot.descriptor.key.version,
            descriptor_activation_height: snapshot.descriptor.activation_height,
            tip_height: snapshot.tip.height,
            tip_block_hash: snapshot.tip.block_hash,
            state_root: snapshot.state_root,
            oldest_rewind_height: snapshot.oldest_rewind_height,
            payload: snapshot.payload,
        }
    }
}

impl TryFrom<StoredApplicationSnapshot> for ApplicationSnapshot {
    type Error = String;

    fn try_from(stored: StoredApplicationSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            format_version: stored.format_version,
            descriptor: coppice::application::ApplicationDescriptor {
                key: coppice::application::ApplicationKey::new(
                    coppice::application::ApplicationId::from_bytes(stored.descriptor_id),
                    stored.descriptor_version,
                ),
                activation_height: stored.descriptor_activation_height,
            },
            tip: coppice::application::ApplicationTip {
                height: stored.tip_height,
                block_hash: stored.tip_block_hash,
            },
            state_root: stored.state_root,
            oldest_rewind_height: stored.oldest_rewind_height,
            payload: stored.payload,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredNamesWallet {
    format_version: u32,
    config: NamesWalletConfig,
    core_snapshot: Option<Vec<u8>>,
    application_snapshot: Option<StoredApplicationSnapshot>,
    /// Wallet-local registration workflow metadata. The nonce is public
    /// randomness; the COMMIT secret is derived from the wallet seed only
    /// while authorizing an operation and is never persisted here.
    #[serde(default)]
    registrations: Vec<StoredRegistration>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredRegistration {
    pub account_uuid: String,
    pub name: String,
    pub record: Vec<u8>,
    pub nonce: [u8; 32],
    pub commitment: [u8; 32],
    #[serde(default)]
    pub send_flow_id: Option<String>,
    #[serde(default)]
    pub bond_txid: Option<[u8; 32]>,
    #[serde(default)]
    pub bond_output_index: Option<u32>,
    pub phase: String,
    pub commit_txid: Option<[u8; 32]>,
    pub reveal_txid: Option<[u8; 32]>,
}

impl StoredNamesWallet {
    fn configured(config: NamesWalletConfig) -> Self {
        Self {
            format_version: STORE_FORMAT_VERSION,
            config,
            core_snapshot: None,
            application_snapshot: None,
            registrations: Vec::new(),
        }
    }

    fn configured_preserving_local(
        config: NamesWalletConfig,
        registrations: Vec<StoredRegistration>,
    ) -> Self {
        Self {
            format_version: STORE_FORMAT_VERSION,
            config,
            core_snapshot: None,
            application_snapshot: None,
            registrations,
        }
    }
}

/// Human-readable wallet-side readiness state.  The application state itself
/// remains opaque to this status projection.
#[derive(Clone, Debug)]
pub struct NamesWalletStatus {
    pub state: String,
    pub message: String,
    pub configured: bool,
    pub tip_height: u64,
    pub names_activation_height: u64,
    pub oldest_rewind_height: u64,
}

/// A compact result suitable for a wallet's name-directory and send flows.
#[derive(Clone, Debug)]
pub struct NamesResolution {
    pub status: String,
    pub record: Option<Vec<u8>>,
    pub payment_address: Option<String>,
    pub sequence: Option<u64>,
    pub lease_expiry: Option<u64>,
    pub terminal_height: Option<u64>,
    pub state_commitment: Option<Vec<u8>>,
    pub tip_height: u64,
    pub candidate_block_probes: u64,
    pub tail_blocks_scanned: u64,
    pub lineage_block_probes: u64,
    pub predecessor_chain_steps: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct NamesLifecycleContext {
    pub params: V1Parameters,
    pub payment_network: PaymentNetwork,
    pub rendezvous_receiver: [u8; 43],
    pub tip_height: u32,
}

pub(crate) fn accepted_commit(
    db_path: &str,
    network: WalletNetwork,
    commitment: [u8; 32],
) -> Result<Option<coppice_names::v1::CommitRef>, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let host = NamesWalletHost::from_stored(network, stored)?
        .ok_or_else(|| "Names must be bootstrapped before registration".to_string())?;
    Ok(host.runtime.applications().pending(commitment))
}

pub(crate) fn accepted_head(
    db_path: &str,
    network: WalletNetwork,
    name: &str,
) -> Result<Option<coppice_names::v1::NameState>, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let host = NamesWalletHost::from_stored(network, stored)?
        .ok_or_else(|| "Names must be bootstrapped before managing names".to_string())?;
    let name_id = coppice_names::v1::state::name_id(name)
        .map_err(|error| format!("invalid Names name: {error:?}"))?;
    Ok(host.runtime.applications().head(name_id).cloned())
}

/// A loaded Core + Names runtime.  The proof verifier is process-cached so a
/// wallet does not regenerate circuit keys for every lookup or sync batch.
pub(crate) struct NamesWalletHost {
    network: WalletNetwork,
    config: NamesWalletConfig,
    runtime: CoppiceRuntime<NamesApplication<OrchardV1ProofVerifier>>,
    registrations: Vec<StoredRegistration>,
}

impl NamesWalletHost {
    fn from_checkpoint(
        network: WalletNetwork,
        config: NamesWalletConfig,
        checkpoint: CoreReplayActivationCheckpoint,
        registrations: Vec<StoredRegistration>,
    ) -> Result<Self, String> {
        let parameters = config.validated_core_parameters(network)?;
        let replay_config =
            CoreReplayConfiguration::new(config.runtime_activation_height, config.retention_blocks)
                .map_err(|error| format!("invalid Core replay configuration: {error:?}"))?;
        let core_replay = CoreReplay::new(replay_config, checkpoint.clone())
            .map_err(|error| format!("invalid Core activation checkpoint: {error:?}"))?;
        let core = coppice::runtime::CoreRuntime::new(parameters, core_replay)
            .map_err(|error| format!("cannot construct Core runtime: {error:?}"))?;
        let app = NamesApplication::new(
            config.names,
            ChainTip {
                height: checkpoint.height,
                block_hash: checkpoint.block_hash,
            },
            proof_verifier(),
            config.retention_blocks,
        )
        .map_err(|error| format!("cannot construct Names application: {error:?}"))?;
        let runtime = CoppiceRuntime::new(core, app)
            .map_err(|error| format!("cannot compose Coppice runtime: {error:?}"))?;
        Ok(Self {
            network,
            config,
            runtime,
            registrations,
        })
    }

    fn from_stored(
        network: WalletNetwork,
        stored: StoredNamesWallet,
    ) -> Result<Option<Self>, String> {
        if stored.format_version != STORE_FORMAT_VERSION {
            return Err(format!(
                "unsupported Coppice Names sidecar format {}",
                stored.format_version
            ));
        }
        let config = stored.config.clone();
        let registrations = stored.registrations.clone();
        let parameters = config.validated_core_parameters(network)?;
        let Some(core_snapshot) = stored.core_snapshot else {
            if stored.application_snapshot.is_some() {
                return Err(
                    "Names sidecar has an application checkpoint without Core state".into(),
                );
            }
            return Ok(None);
        };
        let application_snapshot = stored
            .application_snapshot
            .ok_or_else(|| "Names sidecar has Core state without application state".to_string())?
            .try_into()?;
        let replay_config =
            CoreReplayConfiguration::new(config.runtime_activation_height, config.retention_blocks)
                .map_err(|error| format!("invalid Core replay configuration: {error:?}"))?;
        let core =
            coppice::runtime::CoreRuntime::load_snapshot(parameters, replay_config, &core_snapshot)
                .map_err(|error| format!("invalid persisted Core runtime: {error:?}"))?;
        let app = NamesApplication::from_snapshot_at_runtime(
            application_snapshot,
            proof_verifier(),
            config.retention_blocks,
            config.runtime_activation_height,
        )
        .map_err(|error| format!("invalid persisted Names application: {error:?}"))?;
        if app.params() != config.names {
            return Err("persisted Names parameters do not match wallet configuration".into());
        }
        let runtime = CoppiceRuntime::new(core, app)
            .map_err(|error| format!("persisted Core/Names tips do not match: {error:?}"))?;
        Ok(Some(Self {
            network,
            config,
            runtime,
            registrations,
        }))
    }

    fn to_stored(&self) -> Result<StoredNamesWallet, String> {
        let core_snapshot = self
            .runtime
            .core()
            .save_snapshot()
            .map_err(|error| format!("save Core runtime snapshot: {error:?}"))?;
        let application_snapshot = self
            .runtime
            .applications()
            .save_application_snapshot()
            .map_err(|error| format!("save Names application snapshot: {error:?}"))?
            .into();
        Ok(StoredNamesWallet {
            format_version: STORE_FORMAT_VERSION,
            config: self.config.clone(),
            core_snapshot: Some(core_snapshot),
            application_snapshot: Some(application_snapshot),
            registrations: self.registrations.clone(),
        })
    }

    fn status(&self) -> NamesWalletStatus {
        let tip = self.runtime.tip();
        NamesWalletStatus {
            state: "ready".into(),
            message: String::new(),
            configured: true,
            tip_height: u64::from(tip.height),
            names_activation_height: u64::from(self.config.names.activation_height),
            oldest_rewind_height: u64::from(self.runtime.oldest_rewind_height()),
        }
    }

    pub(crate) fn managed_heads(&self) -> Vec<(String, coppice_names::v1::NameState)> {
        self.registrations
            .iter()
            .filter_map(|registration| {
                let name_id = coppice_names::v1::state::name_id(&registration.name).ok()?;
                self.runtime
                    .applications()
                    .head(name_id)
                    .cloned()
                    .map(|head| (registration.account_uuid.clone(), head))
            })
            .collect()
    }

    pub(crate) fn tip_height(&self) -> u32 {
        self.runtime.tip().height
    }

    pub(crate) fn params(&self) -> V1Parameters {
        self.config.names
    }

    pub(crate) fn can_apply_start(&self, start: u32) -> bool {
        self.runtime
            .tip()
            .height
            .checked_add(1)
            .is_some_and(|next| next == start)
    }

    /// Applies a scanner-accepted compact batch atomically to Core + Names.
    /// Full transactions are fetched only for Core rendezvous candidates.
    pub(crate) async fn apply_compact_blocks(
        &mut self,
        client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
        blocks: Vec<CompactBlock>,
    ) -> Result<(), String> {
        let Some(first) = blocks.first() else {
            return Ok(());
        };
        let first_height = u32::try_from(first.height)
            .map_err(|_| "Names batch height exceeds u32".to_string())?;
        if !self.can_apply_start(first_height) {
            return Err(format!(
                "Names host tip {} is not immediately before batch {}",
                self.runtime.tip().height,
                first_height
            ));
        }
        let mut txids = BTreeSet::new();
        for block in &blocks {
            for tx in &block.vtx {
                let txid = exact_32(&tx.txid)
                    .ok_or_else(|| "compact transaction has a non-32-byte txid".to_string())?;
                if compact_tx_is_rendezvous(tx, self.runtime.core().rendezvous())? {
                    txids.insert(txid);
                }
            }
        }
        let full_transactions = fetch_full_transactions(client, &txids).await?;
        let mut source = MapFullTransactionSource(full_transactions);
        let mut candidate = self.runtime.clone();
        for block in &blocks {
            coppice_librustzcash::apply_compact_block(
                &self.network,
                &mut candidate,
                block,
                &mut source,
            )
            .map_err(|error| format!("apply canonical Names block: {error:?}"))?;
        }
        self.runtime = candidate;
        Ok(())
    }

    /// Builds the exact canonical source required by a bounded fresh lookup.
    /// Compact metadata is never treated as an operation carrier until the
    /// corresponding full transaction has passed txid and Ironwood-action
    /// equality checks.
    async fn canonical_source(
        &self,
        client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
        start: u32,
        end: u32,
    ) -> Result<AcquiredCanonicalSource, String> {
        let blocks = download_range(client, self.network, start, end).await?;
        let mut txids = BTreeSet::new();
        for block in &blocks {
            for tx in &block.vtx {
                if compact_tx_is_rendezvous(tx, self.runtime.core().rendezvous())? {
                    txids.insert(
                        exact_32(&tx.txid).ok_or_else(|| {
                            "compact transaction has a non-32-byte txid".to_string()
                        })?,
                    );
                }
            }
        }
        let full_transactions = fetch_full_transactions(client, &txids).await?;
        let blocks =
            build_canonical_source(self.network, &self.runtime, blocks, &full_transactions)?;
        AcquiredCanonicalSource::new(blocks, end)
    }

    pub(crate) async fn resolve(
        &self,
        client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
        name: &str,
        tip_height: u32,
    ) -> Result<NamesResolution, String> {
        if tip_height < self.config.names.activation_height {
            return Ok(NamesResolution {
                status: "missing".into(),
                record: None,
                payment_address: None,
                sequence: None,
                lease_expiry: None,
                terminal_height: None,
                state_commitment: None,
                tip_height: u64::from(tip_height),
                candidate_block_probes: 0,
                tail_blocks_scanned: 0,
                lineage_block_probes: 0,
                predecessor_chain_steps: 0,
            });
        }
        let max_window = self
            .config
            .names
            .max_anchor_age()
            .map_err(|error| format!("invalid Names parameters: {error:?}"))?
            .max(
                self.config
                    .names
                    .reset_horizon()
                    .map_err(|error| format!("invalid Names parameters: {error:?}"))?,
            );
        if max_window > MAX_RESOLUTION_BLOCKS {
            return Err("Names resolver window exceeds wallet acquisition bound".into());
        }
        let start = tip_height
            .saturating_sub(max_window)
            .max(self.config.names.activation_height);
        let mut source = self.canonical_source(client, start, tip_height).await?;
        let result = loop {
            source.clear_missing();
            match self.runtime.applications().resolve_fresh(name, &source) {
                Ok(result) => break result,
                Err(ResolveError::InvalidLineage) => {
                    let missing = source.take_missing();
                    if missing.is_empty() {
                        return Err(
                            "fresh Names resolution found malformed canonical history".into()
                        );
                    }
                    let missing = missing
                        .into_iter()
                        .filter(|height| !source.contains(*height))
                        .collect::<Vec<_>>();
                    if missing.is_empty() {
                        return Err(
                            "fresh Names resolution found inconsistent canonical history".into(),
                        );
                    }
                    if missing.iter().any(|height| {
                        *height < self.config.names.activation_height || *height > tip_height
                    }) {
                        return Err(
                            "fresh Names resolver requested history outside its canonical range"
                                .into(),
                        );
                    }
                    if source.len().saturating_add(missing.len()) > MAX_RESOLUTION_BLOCKS as usize {
                        return Err(
                            "Names resolver history exceeds wallet acquisition bound".into()
                        );
                    }
                    for (range_start, range_end) in contiguous_ranges(&missing) {
                        let acquired = self
                            .canonical_source(client, range_start, range_end)
                            .await?;
                        source.extend(acquired)?;
                    }
                }
                Err(error) => {
                    return Err(format!("fresh Names resolution failed: {error:?}"));
                }
            }
        };
        let canonical_tip_hash = sync_engine::get_compact_block_hash(client, u64::from(tip_height))
            .await
            .map_err(|error| error.to_string())?;
        if source.tip.block_hash != canonical_tip_hash.0 {
            return Err("fresh Names source tip changed during acquisition".into());
        }
        Ok(project_resolution(
            result,
            tip_height,
            self.config.payment_network(),
        ))
    }
}

/// A branch-scoped canonical source that records older blocks requested by
/// recursive lineage authentication. The wallet initially acquires only the
/// protocol-sized tail and then fills exact missing history ranges. This keeps
/// the common lookup bounded without misclassifying a legitimate older
/// predecessor as source corruption or blindly replaying from activation.
struct AcquiredCanonicalSource {
    tip: ChainTip,
    blocks: BTreeMap<u32, CanonicalBlock>,
    missing: RefCell<BTreeSet<u32>>,
}

impl AcquiredCanonicalSource {
    fn new(blocks: BTreeMap<u32, CanonicalBlock>, tip_height: u32) -> Result<Self, String> {
        let tip = blocks
            .get(&tip_height)
            .ok_or_else(|| "canonical source omitted its requested tip".to_string())?
            .tip();
        Ok(Self {
            tip,
            blocks,
            missing: RefCell::new(BTreeSet::new()),
        })
    }

    fn clear_missing(&self) {
        self.missing.borrow_mut().clear();
    }

    fn take_missing(&self) -> BTreeSet<u32> {
        std::mem::take(&mut *self.missing.borrow_mut())
    }

    fn contains(&self, height: u32) -> bool {
        self.blocks.contains_key(&height)
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }

    fn extend(&mut self, other: Self) -> Result<(), String> {
        let mut candidate = self.blocks.clone();
        for (height, block) in other.blocks {
            if height > self.tip.height {
                return Err("canonical lineage acquisition exceeded the fixed lookup tip".into());
            }
            if let Some(existing) = candidate.get(&height) {
                if existing != &block {
                    return Err(format!("canonical source changed at block {height}"));
                }
                continue;
            }
            if let Some(previous_height) = height.checked_sub(1) {
                if let Some(previous) = candidate.get(&previous_height) {
                    if previous.block_hash != block.prev_block_hash {
                        return Err(format!("canonical source fork before block {height}"));
                    }
                }
            }
            if let Some(next_height) = height.checked_add(1) {
                if let Some(next) = candidate.get(&next_height) {
                    if block.block_hash != next.prev_block_hash {
                        return Err(format!("canonical source fork after block {height}"));
                    }
                }
            }
            candidate.insert(height, block);
        }
        self.blocks = candidate;
        Ok(())
    }
}

impl CanonicalSource for AcquiredCanonicalSource {
    fn tip(&self) -> ChainTip {
        self.tip
    }

    fn block(&self, height: u32) -> Option<CanonicalBlock> {
        match self.blocks.get(&height) {
            Some(block) => Some(block.clone()),
            None => {
                self.missing.borrow_mut().insert(height);
                None
            }
        }
    }
}

fn contiguous_ranges(heights: &[u32]) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let Some(&first) = heights.first() else {
        return ranges;
    };
    let mut start = first;
    let mut end = first;
    for &height in &heights[1..] {
        if end.checked_add(1) == Some(height) {
            end = height;
        } else {
            ranges.push((start, end));
            start = height;
            end = height;
        }
    }
    ranges.push((start, end));
    ranges
}

impl NamesWalletHost {
    pub(crate) fn rewind_to(&mut self, height: u32) -> Result<(), String> {
        self.runtime
            .rewind_to(height)
            .map_err(|error| format!("rewind Names runtime: {error:?}"))
    }
}

/// Configures a sidecar without pretending that it is bootstrapped. Existing
/// state is preserved only when its exact deployment configuration matches.
#[allow(clippy::too_many_arguments)]
pub(crate) fn configure(
    db_path: &str,
    network: WalletNetwork,
    runtime_activation_height: u32,
    names_activation_height: u32,
    epoch_size: u32,
    commit_ttl_blocks: u32,
    refresh_deadline_blocks: u32,
    lease_duration_blocks: u32,
    grace_period_blocks: u32,
    reuse_delay_blocks: u32,
    max_record_bytes: usize,
    minimum_bond_zatoshis: u64,
    retention_blocks: u32,
    network_domain: String,
    rendezvous_ivk_hex: String,
    rendezvous_receiver_hex: String,
) -> Result<NamesWalletStatus, String> {
    let config = NamesWalletConfig::from_api(
        network,
        runtime_activation_height,
        names_activation_height,
        epoch_size,
        commit_ttl_blocks,
        refresh_deadline_blocks,
        lease_duration_blocks,
        grace_period_blocks,
        reuse_delay_blocks,
        max_record_bytes,
        minimum_bond_zatoshis,
        retention_blocks,
        network_domain,
        rendezvous_ivk_hex,
        rendezvous_receiver_hex,
    )?;
    let path = sidecar_path(db_path);
    let stored = read_stored(&path)?;
    if let Some(existing) = stored {
        if existing.config != config {
            return Err("cannot change Names deployment configuration while state exists".into());
        }
        if let Some(host) = NamesWalletHost::from_stored(network, existing)? {
            return Ok(host.status());
        }
    }
    write_stored(&path, &StoredNamesWallet::configured(config.clone()))?;
    Ok(NamesWalletStatus {
        state: "needs_bootstrap".into(),
        message: "exact-name resolution is available; canonical bootstrap is required for complete Names state".into(),
        configured: true,
        tip_height: 0,
        names_activation_height: u64::from(config.names.activation_height),
        oldest_rewind_height: 0,
    })
}

pub(crate) fn status(db_path: &str, network: WalletNetwork) -> Result<NamesWalletStatus, String> {
    let Some(stored) = read_stored(&sidecar_path(db_path))? else {
        return Ok(NamesWalletStatus {
            state: "disabled".into(),
            message: "Names is not configured for this wallet".into(),
            configured: false,
            tip_height: 0,
            names_activation_height: 0,
            oldest_rewind_height: 0,
        });
    };
    let activation = u64::from(stored.config.names.activation_height);
    match NamesWalletHost::from_stored(network, stored) {
        Ok(Some(host)) => Ok(host.status()),
        Ok(None) => Ok(NamesWalletStatus {
            state: "needs_bootstrap".into(),
            message: "exact-name resolution is available; canonical bootstrap is required for complete Names state".into(),
            configured: true,
            tip_height: 0,
            names_activation_height: activation,
            oldest_rewind_height: 0,
        }),
        Err(error) => Ok(NamesWalletStatus {
            state: "corrupt".into(),
            message: error,
            configured: true,
            tip_height: 0,
            names_activation_height: activation,
            oldest_rewind_height: 0,
        }),
    }
}

pub(crate) fn lifecycle_context(
    db_path: &str,
    network: WalletNetwork,
) -> Result<NamesLifecycleContext, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let receiver: [u8; 43] = stored
        .config
        .rendezvous_receiver
        .clone()
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("Names rendezvous receiver has {} bytes", bytes.len()))?;
    let host = NamesWalletHost::from_stored(network, stored)?
        .ok_or_else(|| "Names must be bootstrapped before registration".to_string())?;
    Ok(NamesLifecycleContext {
        params: host.config.names,
        payment_network: host.config.payment_network(),
        rendezvous_receiver: receiver,
        tip_height: host.runtime.tip().height,
    })
}

pub(crate) fn store_registration(
    db_path: &str,
    registration: StoredRegistration,
) -> Result<(), String> {
    let path = sidecar_path(db_path);
    let mut stored = read_stored(&path)?.ok_or_else(|| "Names is not configured".to_string())?;
    if stored.registrations.iter().any(|existing| {
        existing.account_uuid == registration.account_uuid && existing.name == registration.name
    }) {
        return Err("this wallet account already has a registration workflow for that name".into());
    }
    stored.registrations.push(registration);
    write_stored(&path, &stored)
}

pub(crate) fn registrations(db_path: &str) -> Result<Vec<StoredRegistration>, String> {
    Ok(read_stored(&sidecar_path(db_path))?
        .map(|stored| stored.registrations)
        .unwrap_or_default())
}

pub(crate) fn registration(
    db_path: &str,
    account_uuid: &str,
    name: &str,
) -> Result<Option<StoredRegistration>, String> {
    Ok(read_stored(&sidecar_path(db_path))?.and_then(|stored| {
        stored.registrations.into_iter().find(|registration| {
            registration.account_uuid == account_uuid && registration.name == name
        })
    }))
}

pub(crate) fn replace_registration(
    db_path: &str,
    registration: StoredRegistration,
) -> Result<(), String> {
    let path = sidecar_path(db_path);
    let mut stored = read_stored(&path)?.ok_or_else(|| "Names is not configured".to_string())?;
    let existing = stored
        .registrations
        .iter_mut()
        .find(|existing| {
            existing.account_uuid == registration.account_uuid && existing.name == registration.name
        })
        .ok_or_else(|| "Names registration workflow is unavailable".to_string())?;
    *existing = registration;
    write_stored(&path, &stored)
}

pub(crate) fn managed_registrations(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<Vec<StoredRegistration>, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let host = NamesWalletHost::from_stored(network, stored.clone())?;
    let mut registrations = stored
        .registrations
        .into_iter()
        .filter(|registration| registration.account_uuid == account_uuid)
        .collect::<Vec<_>>();
    if let Some(host) = host {
        for registration in &mut registrations {
            if let Ok(name_id) = coppice_names::v1::state::name_id(&registration.name) {
                if let Some(head) = host.runtime.applications().head(name_id) {
                    registration.record = head.data.record.clone();
                    registration.phase = if head.abandoned_height.is_some() {
                        "abandoned"
                    } else {
                        match head.data.status {
                            coppice_names::v1::StateStatus::Released => "released",
                            coppice_names::v1::StateStatus::Active
                                if host.runtime.tip().height >= head.data.lease_expiry =>
                            {
                                "expired"
                            }
                            coppice_names::v1::StateStatus::Active => "active",
                        }
                    }
                    .to_string();
                    continue;
                }
            }
            if host
                .runtime
                .applications()
                .pending(registration.commitment)
                .is_some()
            {
                registration.phase = "commit_accepted".to_string();
            }
        }
    }
    registrations.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(registrations)
}

pub(crate) fn take_cancelled_registration(
    db_path: &str,
    send_flow_id: &str,
) -> Result<Option<StoredRegistration>, String> {
    let path = sidecar_path(db_path);
    let Some(mut stored) = read_stored(&path)? else {
        return Ok(None);
    };
    let Some(index) = stored
        .registrations
        .iter()
        .position(|registration| registration.send_flow_id.as_deref() == Some(send_flow_id))
    else {
        return Ok(None);
    };
    let registration = stored.registrations.remove(index);
    write_stored(&path, &stored)?;
    Ok(Some(registration))
}

pub(crate) fn record_reveal_broadcast(
    db_path: &str,
    account_uuid: &str,
    name: &str,
    txid: [u8; 32],
) -> Result<(), String> {
    let path = sidecar_path(db_path);
    let mut stored = read_stored(&path)?.ok_or_else(|| "Names is not configured".to_string())?;
    let registration = stored
        .registrations
        .iter_mut()
        .find(|registration| registration.account_uuid == account_uuid && registration.name == name)
        .ok_or_else(|| "Names registration workflow is unavailable".to_string())?;
    registration.phase = "reveal_broadcast".to_string();
    registration.reveal_txid = Some(txid);
    registration.send_flow_id = None;
    write_stored(&path, &stored)
}

pub(crate) async fn bootstrap(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
) -> Result<NamesWalletStatus, String> {
    let path = sidecar_path(db_path);
    let stored = read_stored(&path)?.ok_or_else(|| "Names is not configured".to_string())?;
    let mut client = sync_engine::open_lwd_channel(lightwalletd_url)
        .await
        .map_err(|error| error.to_string())?;
    let tip = sync_engine::get_latest_block(&mut client)
        .await
        .map_err(|error| error.to_string())?;
    let tip_height =
        u32::try_from(tip.height).map_err(|_| "lightwalletd tip exceeds u32".to_string())?;

    let mut host = if let Some(host) = NamesWalletHost::from_stored(network, stored.clone())? {
        host
    } else {
        let base = stored
            .config
            .runtime_activation_height
            .checked_sub(1)
            .ok_or_else(|| "runtime activation has no pre-activation base".to_string())?;
        if tip_height < base {
            return Err(format!(
                "lightwalletd tip {tip_height} is before Coppice runtime base {base}"
            ));
        }
        let checkpoint = activation_checkpoint(&mut client, base).await?;
        NamesWalletHost::from_checkpoint(network, stored.config, checkpoint, stored.registrations)?
    };

    let mut host_tip = host.runtime.tip();
    if host_tip.height > 0 && host_tip.height <= tip_height {
        let canonical_host_hash =
            sync_engine::get_compact_block_hash(&mut client, u64::from(host_tip.height))
                .await
                .map_err(|error| error.to_string())?;
        if canonical_host_hash.0 != host_tip.block_hash {
            let config = host.config.clone();
            let registrations = host.registrations.clone();
            let base = config
                .runtime_activation_height
                .checked_sub(1)
                .ok_or_else(|| "runtime activation has no pre-activation base".to_string())?;
            let checkpoint = activation_checkpoint(&mut client, base).await?;
            host = NamesWalletHost::from_checkpoint(network, config, checkpoint, registrations)?;
            host_tip = host.runtime.tip();
        }
    }
    if host_tip.height > tip_height {
        return Err(
            "persisted Names state is ahead of lightwalletd; rebootstrap is required".into(),
        );
    }
    if host_tip.height == tip_height {
        let canonical_tip_hash =
            sync_engine::get_compact_block_hash(&mut client, u64::from(tip_height))
                .await
                .map_err(|error| error.to_string())?;
        if canonical_tip_hash.0 != host_tip.block_hash {
            write_stored(
                &path,
                &StoredNamesWallet::configured_preserving_local(
                    host.config.clone(),
                    host.registrations.clone(),
                ),
            )?;
            return Err(
                "persisted Names tip conflicts with lightwalletd; rebootstrap is required".into(),
            );
        }
        persist_host_preserving_workflows(&path, &host)?;
        return Ok(host.status());
    }

    let mut next = host_tip.height.saturating_add(1);
    while next <= tip_height {
        let end = next
            .saturating_add(ACQUISITION_BATCH_BLOCKS.saturating_sub(1))
            .min(tip_height);
        let blocks = sync_engine::download_blocks_vec(
            &mut client,
            BlockHeight::from_u32(next),
            BlockHeight::from_u32(end),
            network,
        )
        .await
        .map_err(|error| error.to_string())?;
        host.apply_compact_blocks(&mut client, blocks).await?;
        persist_host_preserving_workflows(&path, &host)?;
        next = end.saturating_add(1);
    }
    let canonical_tip_hash =
        sync_engine::get_compact_block_hash(&mut client, u64::from(tip_height))
            .await
            .map_err(|error| error.to_string())?;
    if canonical_tip_hash.0 != host.runtime.tip().block_hash {
        write_stored(
            &path,
            &StoredNamesWallet::configured_preserving_local(
                host.config.clone(),
                host.registrations.clone(),
            ),
        )?;
        return Err("bootstrap tip changed or failed canonical identity check".into());
    }
    Ok(host.status())
}

pub(crate) async fn resolve_name(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    name: &str,
) -> Result<NamesResolution, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let mut client = sync_engine::open_lwd_channel(lightwalletd_url)
        .await
        .map_err(|error| error.to_string())?;
    let tip = sync_engine::get_latest_block(&mut client)
        .await
        .map_err(|error| error.to_string())?;
    let tip_height =
        u32::try_from(tip.height).map_err(|_| "lightwalletd tip exceeds u32".to_string())?;
    // Exact-name resolution is deliberately independent of full application
    // bootstrap. If no durable replay exists yet, construct only the
    // authenticated Core/Names activation base needed to host FreshResolver;
    // do not replay or persist activation-to-tip state as a hidden cost of a
    // single lookup.
    let host = match NamesWalletHost::from_stored(network, stored.clone())? {
        Some(host) => host,
        None => {
            let base = stored
                .config
                .runtime_activation_height
                .checked_sub(1)
                .ok_or_else(|| "runtime activation has no pre-activation base".to_string())?;
            if tip_height < base {
                return Err(format!(
                    "lightwalletd tip {tip_height} is before Coppice runtime base {base}"
                ));
            }
            let checkpoint = activation_checkpoint(&mut client, base).await?;
            NamesWalletHost::from_checkpoint(
                network,
                stored.config,
                checkpoint,
                stored.registrations,
            )?
        }
    };
    host.resolve(&mut client, name, tip_height).await
}

pub(crate) fn load_for_sync(
    db_path: &str,
    network: WalletNetwork,
) -> Result<Option<NamesWalletHost>, String> {
    let Some(stored) = read_stored(&sidecar_path(db_path))? else {
        return Ok(None);
    };
    NamesWalletHost::from_stored(network, stored)
}

pub(crate) fn persist_for_sync(db_path: &str, host: &NamesWalletHost) -> Result<(), String> {
    persist_host_preserving_workflows(&sidecar_path(db_path), host)
}

fn persist_host_preserving_workflows(path: &Path, host: &NamesWalletHost) -> Result<(), String> {
    let mut checkpoint = host.to_stored()?;
    // Sync and explicit bootstrap can run while the UI starts or advances a
    // registration. Runtime snapshots are derived state; wallet workflows are
    // local custody metadata and the latest durable copy must win.
    if let Some(current) = read_stored(path)? {
        if current.config != checkpoint.config {
            return Err("Names configuration changed while persisting replay state".to_string());
        }
        checkpoint.registrations = current.registrations;
    }
    write_stored(path, &checkpoint)
}

/// A persisted application checkpoint contains no undo journal. If a wallet
/// reorg lands outside the in-memory host history, discard only the derived
/// Names checkpoint and leave the wallet's canonical DB untouched; the next
/// explicit Names bootstrap will rebuild it from the configured activation
/// boundary.
pub(crate) fn invalidate_after_reorg(
    db_path: &str,
    hosts: &mut Option<NamesWalletHost>,
    rewind_height: u32,
) {
    let Some(mut host) = hosts.take() else {
        return;
    };
    match host.rewind_to(rewind_height) {
        Ok(()) => {
            if let Err(error) = persist_for_sync(db_path, &host) {
                log::warn!(
                    "[{}] Names host rewind persisted state failed; requiring rebootstrap: {error}",
                    super::sync_engine::elapsed()
                );
                let _ = write_stored(
                    &sidecar_path(db_path),
                    &StoredNamesWallet::configured_preserving_local(
                        host.config.clone(),
                        host.registrations.clone(),
                    ),
                );
            } else {
                *hosts = Some(host);
            }
        }
        Err(error) => {
            log::warn!(
                "[{}] Names host cannot rewind across wallet reorg ({error}); requiring rebootstrap",
                super::sync_engine::elapsed()
            );
            if let Err(write_error) = write_stored(
                &sidecar_path(db_path),
                &StoredNamesWallet::configured_preserving_local(
                    host.config.clone(),
                    host.registrations.clone(),
                ),
            ) {
                log::error!(
                    "[{}] failed to persist Names rebootstrap marker: {write_error}",
                    super::sync_engine::elapsed()
                );
            }
        }
    }
}

/// Disables the derived host after an application/source failure while
/// preserving its deployment configuration for an explicit retry.
pub(crate) fn disable_after_error(db_path: &str, hosts: &mut Option<NamesWalletHost>) {
    let Some(host) = hosts.take() else {
        return;
    };
    if let Err(error) = write_stored(
        &sidecar_path(db_path),
        &StoredNamesWallet::configured_preserving_local(
            host.config.clone(),
            host.registrations.clone(),
        ),
    ) {
        log::error!(
            "[{}] failed to persist Names disabled marker: {error}",
            super::sync_engine::elapsed()
        );
    }
}

fn project_resolution(
    result: ResolutionResult,
    tip_height: u32,
    payment_network: PaymentNetwork,
) -> NamesResolution {
    let state = result.state.as_ref();
    let payable = result.status == ResolutionStatus::Active;
    let (record, payment_address) = state
        .map(|state| {
            let record = state.data.record.clone();
            let address = payable
                .then(|| PaymentRecord::decode(&record, payment_network).ok())
                .flatten()
                .map(|record| record.address().to_owned());
            (Some(record), address)
        })
        .unwrap_or((None, None));
    NamesResolution {
        status: resolution_status_name(result.status).into(),
        record,
        payment_address,
        sequence: state.map(|state| state.data.sequence),
        lease_expiry: state.map(|state| u64::from(state.data.lease_expiry)),
        terminal_height: state.map(|state| u64::from(state.data.terminal_height)),
        state_commitment: state.map(|state| state.commitment.to_vec()),
        tip_height: u64::from(tip_height),
        candidate_block_probes: u64::from(result.stats.candidate_block_probes),
        tail_blocks_scanned: u64::from(result.stats.tail_blocks_scanned),
        lineage_block_probes: u64::from(result.stats.lineage_block_probes),
        predecessor_chain_steps: u64::from(result.stats.predecessor_chain_steps),
    }
}

fn resolution_status_name(status: ResolutionStatus) -> &'static str {
    match status {
        ResolutionStatus::Active => "active",
        ResolutionStatus::Stale => "stale",
        ResolutionStatus::Grace => "grace",
        ResolutionStatus::Released => "released",
        ResolutionStatus::Abandoned => "abandoned",
        ResolutionStatus::Expired => "expired",
        ResolutionStatus::Missing => "missing",
    }
}

fn proof_verifier() -> Arc<OrchardV1ProofVerifier> {
    static VERIFIER: OnceLock<Arc<OrchardV1ProofVerifier>> = OnceLock::new();
    VERIFIER
        .get_or_init(|| Arc::new(OrchardV1ProofVerifier::new()))
        .clone()
}

fn core_network(network: WalletNetwork) -> ZcashNetwork {
    match network {
        WalletNetwork::Main => ZcashNetwork::Main,
        WalletNetwork::Test => ZcashNetwork::Test,
        WalletNetwork::Regtest => ZcashNetwork::Regtest,
    }
}

fn network_code(network: WalletNetwork) -> u8 {
    match network {
        WalletNetwork::Main => 0,
        WalletNetwork::Test => 1,
        WalletNetwork::Regtest => 2,
    }
}

fn decode_fixed<const N: usize>(hex_value: &str, label: &str) -> Result<[u8; N], String> {
    let bytes = hex::decode(hex_value).map_err(|error| format!("{label} is not hex: {error}"))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("{label} must be exactly {N} bytes, got {}", bytes.len()))
}

fn exact_32(bytes: &[u8]) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}

fn compact_tx_is_rendezvous(
    tx: &CompactTx,
    rendezvous: &coppice::carrier::CoreRendezvous,
) -> Result<bool, String> {
    let mut candidate = false;
    for (index, encoded) in tx.ironwood_actions.iter().enumerate() {
        let action = CompactAction::try_from(encoded)
            .map_err(|_| format!("invalid compact Ironwood action {index}"))?;
        candidate |= rendezvous.compact_action_is_rendezvous(&action);
    }
    Ok(candidate)
}

async fn fetch_full_transactions(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    txids: &BTreeSet<[u8; 32]>,
) -> Result<BTreeMap<[u8; 32], Vec<u8>>, String> {
    let mut result = BTreeMap::new();
    let mut acquired_bytes = 0usize;
    for txid in txids {
        let raw: RawTransaction = sync_engine::get_transaction(client, txid.to_vec())
            .await
            .map_err(|status| format!("get_transaction {}: {status}", hex::encode(txid)))?;
        if raw.data.len() > coppice_librustzcash::MAX_FULL_TRANSACTION_BYTES {
            return Err(format!(
                "full transaction {} exceeds {} bytes",
                hex::encode(txid),
                coppice_librustzcash::MAX_FULL_TRANSACTION_BYTES
            ));
        }
        acquired_bytes = acquired_bytes
            .checked_add(raw.data.len())
            .ok_or_else(|| "full transaction acquisition size overflowed".to_string())?;
        if acquired_bytes > MAX_ACQUIRED_FULL_TRANSACTION_BYTES {
            return Err(format!(
                "full transaction acquisition exceeds {MAX_ACQUIRED_FULL_TRANSACTION_BYTES} bytes"
            ));
        }
        result.insert(*txid, raw.data);
    }
    Ok(result)
}

struct MapFullTransactionSource(BTreeMap<[u8; 32], Vec<u8>>);

impl FullTransactionSource for MapFullTransactionSource {
    type Error = String;

    fn full_transaction(&mut self, txid: [u8; 32]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.0.get(&txid).cloned())
    }
}

async fn activation_checkpoint(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    height: u32,
) -> Result<CoreReplayActivationCheckpoint, String> {
    let state: TreeState = sync_engine::get_tree_state(client, u64::from(height))
        .await
        .map_err(|error| error.to_string())?;
    let chain_state = state
        .to_chain_state()
        .map_err(|error| format!("decode activation tree state: {error}"))?;
    if u32::from(chain_state.block_height()) != height {
        return Err(format!(
            "activation tree state is at {}, expected {height}",
            u32::from(chain_state.block_height())
        ));
    }
    let frontier = incrementalmerkletree::frontier::CommitmentTree::from_frontier(
        chain_state.final_ironwood_tree(),
    );
    let ironwood_tree_size = u32::try_from(frontier.size())
        .map_err(|_| "Ironwood activation tree size exceeds u32".to_string())?;
    Ok(CoreReplayActivationCheckpoint {
        height,
        block_hash: chain_state.block_hash().0,
        ironwood_frontier: frontier,
        ironwood_tree_size,
    })
}

async fn download_range(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    network: WalletNetwork,
    start: u32,
    end: u32,
) -> Result<Vec<CompactBlock>, String> {
    let count = end
        .checked_sub(start)
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| "Names resolution range overflowed".to_string())?;
    if count > MAX_RESOLUTION_BLOCKS {
        return Err("Names resolution range exceeds wallet acquisition bound".into());
    }
    let mut result = Vec::with_capacity(count as usize);
    let mut next = start;
    while next <= end {
        let batch_end = next
            .saturating_add(ACQUISITION_BATCH_BLOCKS.saturating_sub(1))
            .min(end);
        let mut batch = sync_engine::download_blocks_vec(
            client,
            BlockHeight::from_u32(next),
            BlockHeight::from_u32(batch_end),
            network,
        )
        .await
        .map_err(|error| error.to_string())?;
        result.append(&mut batch);
        next = batch_end.saturating_add(1);
    }
    Ok(result)
}

fn build_canonical_source(
    network: WalletNetwork,
    runtime: &CoppiceRuntime<NamesApplication<OrchardV1ProofVerifier>>,
    blocks: Vec<CompactBlock>,
    full_transactions: &BTreeMap<[u8; 32], Vec<u8>>,
) -> Result<BTreeMap<u32, CanonicalBlock>, String> {
    let mut source = BTreeMap::new();
    let mut previous_hash = None;
    for block in blocks {
        let height = u32::try_from(block.height).map_err(|_| "compact block height exceeds u32")?;
        let block_hash = exact_32(&block.hash)
            .ok_or_else(|| "compact block hash is not 32 bytes".to_string())?;
        let prev_block_hash = exact_32(&block.prev_hash)
            .ok_or_else(|| "compact block previous hash is not 32 bytes".to_string())?;
        if previous_hash.is_some_and(|hash| hash != prev_block_hash) {
            return Err(format!("compact block chain discontinuity at {height}"));
        }
        previous_hash = Some(block_hash);
        let mut transactions = Vec::with_capacity(block.vtx.len());
        let mut previous_tx_index = None;
        for tx in block.vtx {
            let tx_index = u32::try_from(tx.index)
                .map_err(|_| format!("transaction index exceeds u32 at block {height}"))?;
            if previous_tx_index.is_some_and(|prior| prior >= tx_index) {
                return Err(format!("non-canonical transaction order at block {height}"));
            }
            previous_tx_index = Some(tx_index);
            let txid = exact_32(&tx.txid)
                .ok_or_else(|| format!("transaction txid is not 32 bytes at block {height}"))?;
            let mut actions = Vec::with_capacity(tx.ironwood_actions.len());
            let mut candidate = false;
            for (action_index, encoded) in tx.ironwood_actions.iter().enumerate() {
                let action = CompactAction::try_from(encoded).map_err(|_| {
                    format!("invalid compact action at {height}:{tx_index}:{action_index}")
                })?;
                let action_ref = IronwoodActionRef {
                    action_index: u32::try_from(action_index)
                        .map_err(|_| "Ironwood action index exceeds u32".to_string())?,
                    nullifier: action.nullifier().to_bytes(),
                    commitment: action.cmx().to_bytes(),
                };
                candidate |= runtime
                    .core()
                    .rendezvous()
                    .compact_action_is_rendezvous(&action);
                actions.push(action_ref);
            }
            let operations = if candidate {
                let bytes = full_transactions
                    .get(&txid)
                    .ok_or_else(|| format!("missing full transaction for candidate {txid:?}"))?;
                decode_candidate_operations(network, runtime, height, txid, &tx, bytes)?
            } else {
                Vec::new()
            };
            transactions.push(CanonicalTransaction {
                tx_index,
                txid,
                actions,
                operations,
            });
        }
        source.insert(
            height,
            CanonicalBlock {
                height,
                block_hash,
                prev_block_hash,
                transactions,
            },
        );
    }
    Ok(source)
}

fn decode_candidate_operations(
    network: WalletNetwork,
    runtime: &CoppiceRuntime<NamesApplication<OrchardV1ProofVerifier>>,
    height: u32,
    txid: [u8; 32],
    compact_tx: &CompactTx,
    bytes: &[u8],
) -> Result<Vec<V1Operation>, String> {
    if bytes.is_empty() {
        return Err(format!("candidate transaction {txid:?} is empty"));
    }
    let branch = BranchId::for_height(&network, BlockHeight::from_u32(height));
    let mut cursor = Cursor::new(bytes);
    let transaction = Transaction::read(&mut cursor, branch)
        .map_err(|error| format!("decode candidate transaction {txid:?}: {error}"))?;
    if cursor.position() != bytes.len() as u64 {
        return Err(format!("candidate transaction {txid:?} has trailing bytes"));
    }
    if <[u8; 32]>::from(transaction.txid()) != txid {
        return Err(format!(
            "candidate transaction {txid:?} failed txid authentication"
        ));
    }
    let full_actions = transaction
        .ironwood_bundle()
        .map(|bundle| bundle.actions().iter().collect::<Vec<_>>())
        .unwrap_or_default();
    if full_actions.len() != compact_tx.ironwood_actions.len() {
        return Err(format!(
            "candidate transaction {txid:?} action count mismatch"
        ));
    }
    for (index, (full, compact)) in full_actions
        .iter()
        .zip(&compact_tx.ironwood_actions)
        .enumerate()
    {
        let compact = CompactAction::try_from(compact)
            .map_err(|_| format!("invalid compact action at {height}:{index}"))?;
        if full.nullifier().to_bytes() != compact.nullifier().to_bytes()
            || full.cmx().to_bytes() != compact.cmx().to_bytes()
        {
            return Err(format!(
                "candidate transaction {txid:?} action mismatch at {index}"
            ));
        }
    }
    let inspection = runtime.core().inspect_transaction(&transaction);
    match inspection.message() {
        ApplicationMessageStatus::Message(message)
            if message.key()
                == ApplicationKey::new(names_application_id(), NAMES_APPLICATION_VERSION) =>
        {
            decode_operations(message.payload())
                .map_err(|error| format!("decode Names operations in {txid:?}: {error:?}"))
        }
        ApplicationMessageStatus::Message(_)
        | ApplicationMessageStatus::NotCandidate
        | ApplicationMessageStatus::NoMessage
        | ApplicationMessageStatus::MalformedTransport(_)
        | ApplicationMessageStatus::MalformedEnvelope(_) => Ok(Vec::new()),
    }
}

fn sidecar_path(db_path: &str) -> PathBuf {
    PathBuf::from(format!("{db_path}{STORE_SUFFIX}"))
}

fn read_stored(path: &Path) -> Result<Option<StoredNamesWallet>, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("stat Names sidecar {}: {error}", path.display())),
    };
    if metadata.len() > MAX_STORE_BYTES {
        return Err(format!("Names sidecar exceeds {MAX_STORE_BYTES} bytes"));
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read Names sidecar {}: {error}", path.display())),
    };
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(format!("Names sidecar exceeds {MAX_STORE_BYTES} bytes"));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("decode Names sidecar: {error}"))
}

fn write_stored(path: &Path, stored: &StoredNamesWallet) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(stored).map_err(|error| format!("encode Names sidecar: {error}"))?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(format!("Names sidecar exceeds {MAX_STORE_BYTES} bytes"));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create Names sidecar directory {}: {error}",
            parent.display()
        )
    })?;
    let mut file = tempfile::Builder::new()
        .prefix(".coppice-names-")
        .tempfile_in(parent)
        .map_err(|error| format!("create Names sidecar temporary file: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write Names sidecar: {error}"))?;
    file.as_file()
        .sync_all()
        .map_err(|error| format!("sync Names sidecar: {error}"))?;
    file.persist(path)
        .map_err(|error| format!("replace Names sidecar {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[path = "tests/coppice.rs"]
mod tests;
