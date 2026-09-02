//! Replacement Coppice Names host.
//!
//! This is the wallet host for the replacement-only protocol. It keeps Core
//! application-blind, maintains
//! one exact resolver per locally managed name, and forks all derived state
//! before a wallet scan so the caller can commit it only with the accepted
//! wallet database transaction.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use coppice::{
    carrier::CoreRendezvous,
    identity::{CoreRuntimeParameters, ValidatedCoreRuntimeParameters, ZcashNetwork},
    replay::{CoreReplay, CoreReplayActivationCheckpoint, CoreReplayConfiguration},
    runtime::CoreRuntime,
};
use coppice_librustzcash::{apply_compact_block_with_additional_rendezvous, FullTransactionSource};
use coppice_names::{
    deployment::DeploymentParameters,
    proof::{keygen, OrchardProofVerifier},
    protocol::{CanonicalUa, FieldElement, Name, NameRoute, Network},
    reducer::{Head, Lifecycle},
    resolver::ExactResolver,
    schedule::Parameters as NamesParameters,
    transport::{authenticated_action_position, inspect_exact_name_block},
};
use orchard::note_encryption::CompactAction;
use rand_10::Rng;
use serde::{Deserialize, Serialize};
use zcash_client_backend::proto::service::TreeState;
use zcash_client_backend::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{compact_tx_streamer_client::CompactTxStreamerClient, RawTransaction},
};
use zcash_protocol::consensus::{BlockHeight, Parameters};

use super::{network::WalletNetwork, sync_engine};

const STORE_FORMAT_VERSION: u32 = 2;
const STORE_SUFFIX: &str = ".coppice-names";
const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ACQUIRED_FULL_TRANSACTION_BYTES: usize = 64 * 1024 * 1024;
const ACQUISITION_BATCH_BLOCKS: u32 = 2_000;
const REGTEST_ACTIVATION_HEIGHT: u32 = 2;
const REGTEST_NETWORK_DOMAIN: &str = "coppice-runtime-regtest-v1";
const REGTEST_RENDEZVOUS_IVK_HEX: &str = "65deb2b3ee7ac69020543f40f21122cb6dc1f4201a329fcdf9d5e3bb2dfbbabe29d542352fe36c3c7b24c2989dc9d0000b9e04f444e05dc4538bde395c0e6008";
const REGTEST_RENDEZVOUS_RECEIVER_HEX: &str =
    "9ec59e4d447ba285086cc3456cadf62004a19b6a7989c726daaa9944a6cdbf25f7bfa51afa15b66da53881";

#[derive(Serialize, Deserialize)]
struct StoredRuntimeCheckpoint {
    format_version: u32,
    core: Vec<u8>,
    resolvers: Vec<StoredManagedResolver>,
}

#[derive(Serialize, Deserialize)]
struct StoredManagedResolver {
    name: String,
    resolver: Vec<u8>,
    bond_positions: Vec<([u8; 32], u32)>,
    #[serde(default)]
    bond_origin: Option<BondOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BondOrigin {
    Reveal {
        commit: coppice_names::protocol::CommitRef,
        epoch: u32,
        ua: String,
        action_index: u32,
        action_nullifier: [u8; 32],
    },
    Refresh {
        predecessor: coppice_names::protocol::StateRef,
        epoch: u32,
        ua: String,
        action_index: u32,
        action_nullifier: [u8; 32],
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredNamesWallet {
    format_version: u32,
    config: NamesWalletConfig,
    checkpoint: Option<Vec<u8>>,
    #[serde(default)]
    checkpoint_tag: Option<[u8; 32]>,
    #[serde(default)]
    registrations: Vec<StoredRegistration>,
    #[serde(default)]
    tracked_names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredRegistration {
    pub account_uuid: String,
    pub name: String,
    pub ua: String,
    pub commitment: [u8; 32],
    pub target_epoch: u32,
    pub send_flow_id: Option<String>,
    pub bond_txid: Option<[u8; 32]>,
    pub bond_output_index: Option<u32>,
    pub commit_height: Option<u32>,
    #[serde(default)]
    pub commit_tx_index: Option<u32>,
    pub phase: String,
    pub commit_txid: Option<[u8; 32]>,
    pub reveal_txid: Option<[u8; 32]>,
}

#[derive(Clone, Debug)]
pub struct NamesWalletStatus {
    pub state: String,
    pub message: String,
    pub configured: bool,
    pub tip_height: u64,
    pub names_activation_height: u64,
    pub oldest_rewind_height: u64,
}

#[derive(Clone, Debug)]
pub struct NamesResolution {
    pub status: String,
    pub payment_address: Option<String>,
    pub lease_expiry: Option<u64>,
    pub terminal_height: Option<u64>,
    pub producer: Option<coppice_names::protocol::StateRef>,
    pub tip_height: u64,
    pub compact_blocks_scanned: u64,
}

#[derive(Clone)]
pub(crate) struct NamesLifecycleContext {
    pub parameters: NamesParameters,
    pub deployment: DeploymentParameters,
    pub network: Network,
    pub rendezvous_receiver: [u8; 43],
    pub tip_height: u32,
}

pub(crate) struct ConfiguredNamesMetadata {
    pub parameters: NamesParameters,
    pub tip_height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NamesWalletConfig {
    network_code: u8,
    activation_height: u32,
    retention_blocks: u32,
    network_domain: Vec<u8>,
    rendezvous_ivk: Vec<u8>,
    rendezvous_receiver: Vec<u8>,
}

impl NamesWalletConfig {
    pub(crate) fn from_api(
        network: WalletNetwork,
        activation_height: u32,
        retention_blocks: u32,
        network_domain: String,
        rendezvous_ivk_hex: String,
        rendezvous_receiver_hex: String,
    ) -> Result<Self, String> {
        if activation_height == 0 {
            return Err("Coppice Names activation height must be nonzero".into());
        }
        if retention_blocks == 0 {
            return Err("Names rewind retention must be nonzero".into());
        }
        if network_domain.is_empty() || network_domain.len() > u16::MAX as usize {
            return Err("Coppice network domain must be 1..65535 bytes".into());
        }
        let config = Self {
            network_code: network_code(network),
            activation_height,
            retention_blocks,
            network_domain: network_domain.into_bytes(),
            rendezvous_ivk: decode_fixed::<64>(&rendezvous_ivk_hex, "rendezvous IVK")?.to_vec(),
            rendezvous_receiver: decode_fixed::<43>(
                &rendezvous_receiver_hex,
                "rendezvous receiver",
            )?
            .to_vec(),
        };
        config.validated_core_parameters(network)?;
        config.deployment(network)?;
        Ok(config)
    }

    pub(crate) fn validated_core_parameters(
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
            runtime_activation_height: self.activation_height,
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

    pub(crate) fn deployment(
        &self,
        network: WalletNetwork,
    ) -> Result<DeploymentParameters, String> {
        let core = self.validated_core_parameters(network)?;
        let proof = proof_verifier().identity();
        DeploymentParameters {
            core_runtime_id: core.core_runtime_id(),
            activation_height: self.activation_height,
            epoch_blocks: 1_152,
            window_blocks: 24,
            commit_maturity_blocks: 24,
            commit_ttl_blocks: 192,
            lease_blocks: 250_000,
            cooldown_blocks: 1_152,
            proof,
        }
        .validate()
        .map_err(|error| format!("invalid Names deployment: {error:?}"))
    }

    pub(crate) fn parameters(&self, network: WalletNetwork) -> Result<NamesParameters, String> {
        let deployment = self.deployment(network)?;
        let deployment_id = deployment
            .deployment_id()
            .map_err(|error| format!("derive Names deployment ID: {error:?}"))?;
        Ok(deployment.schedule(deployment_id))
    }

    pub(crate) fn network(&self) -> Network {
        match self.network_code {
            0 => Network::Main,
            1 => Network::Test,
            _ => Network::Regtest,
        }
    }
}

fn deployed_config(
    network: WalletNetwork,
    retention_blocks: u32,
) -> Result<NamesWalletConfig, String> {
    match network {
        WalletNetwork::Regtest => NamesWalletConfig::from_api(
            network,
            REGTEST_ACTIVATION_HEIGHT,
            retention_blocks,
            REGTEST_NETWORK_DOMAIN.into(),
            REGTEST_RENDEZVOUS_IVK_HEX.into(),
            REGTEST_RENDEZVOUS_RECEIVER_HEX.into(),
        ),
        WalletNetwork::Main | WalletNetwork::Test => {
            Err("Coppice Names has no deployment for this network".into())
        }
    }
}

fn validate_deployed_config(
    network: WalletNetwork,
    config: &NamesWalletConfig,
) -> Result<(), String> {
    let expected = deployed_config(network, config.retention_blocks)?;
    if config == &expected {
        Ok(())
    } else {
        Err("Names sidecar deployment identity does not match this wallet build".into())
    }
}

#[derive(Clone)]
struct ManagedResolver {
    resolver: ExactResolver<Arc<OrchardProofVerifier>>,
    bond_positions: BTreeMap<[u8; 32], u32>,
    bond_origin: Option<BondOrigin>,
}

#[derive(Clone)]
pub(crate) struct NamesWalletHost {
    network: WalletNetwork,
    config: NamesWalletConfig,
    core: CoreRuntime,
    resolvers: BTreeMap<String, ManagedResolver>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedResolution {
    pub name: String,
    pub lifecycle: Lifecycle,
    pub head: Option<Head>,
    pub marked_position: Option<u32>,
    pub bond_origin: Option<BondOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedLeaf {
    pub commitment: [u8; 32],
    pub mark: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedBlockDelta {
    pub height: u32,
    pub block_start_position: Option<u32>,
    pub leaves: Vec<ManagedLeaf>,
    pub remove_marks: Vec<u32>,
    pub accepted_commits: Vec<([u8; 32], coppice_names::protocol::CommitRef)>,
}

impl NamesWalletHost {
    pub(crate) fn from_checkpoint(
        network: WalletNetwork,
        config: NamesWalletConfig,
        checkpoint: CoreReplayActivationCheckpoint,
        names: impl IntoIterator<Item = String>,
    ) -> Result<Self, String> {
        let core_parameters = config.validated_core_parameters(network)?;
        let replay_configuration =
            CoreReplayConfiguration::new(config.activation_height, config.retention_blocks)
                .map_err(|error| format!("invalid Core replay configuration: {error:?}"))?;
        let activation_parent_hash = checkpoint.block_hash;
        let replay = CoreReplay::new(replay_configuration, checkpoint)
            .map_err(|error| format!("invalid Core activation checkpoint: {error:?}"))?;
        let core = CoreRuntime::new(core_parameters, replay)
            .map_err(|error| format!("cannot construct Core runtime: {error:?}"))?;
        let parameters = config.parameters(network)?;
        let verifier = proof_verifier();
        let mut resolvers = BTreeMap::new();
        for value in names {
            let name = Name::parse(&value)
                .map_err(|error| format!("invalid managed Names label {value:?}: {error:?}"))?;
            let canonical = name.as_str().to_owned();
            let resolver = ExactResolver::new(
                parameters,
                activation_parent_hash,
                name,
                Arc::clone(&verifier),
            )
            .map_err(|error| format!("cannot construct exact Names resolver: {error:?}"))?;
            if resolvers
                .insert(
                    canonical,
                    ManagedResolver {
                        resolver,
                        bond_positions: BTreeMap::new(),
                        bond_origin: None,
                    },
                )
                .is_some()
            {
                return Err("duplicate managed Names label".into());
            }
        }
        Ok(Self {
            network,
            config,
            core,
            resolvers,
        })
    }

    pub(crate) fn fork(&self) -> Self {
        self.clone()
    }

    /// Stores a branch-bound Core/exact-resolution pair. These bytes remain a
    /// derived cache; the sidecar layer only accepts them while authenticated
    /// by this process and otherwise requires canonical replay.
    pub(crate) fn save_checkpoint(&self) -> Result<Vec<u8>, String> {
        let stored = StoredRuntimeCheckpoint {
            format_version: STORE_FORMAT_VERSION,
            core: self
                .core
                .save_snapshot()
                .map_err(|error| format!("save Core runtime snapshot: {error:?}"))?,
            resolvers: self
                .resolvers
                .iter()
                .map(|(name, managed)| {
                    Ok(StoredManagedResolver {
                        name: name.clone(),
                        resolver: managed
                            .resolver
                            .save_snapshot()
                            .map_err(|error| format!("save exact Names snapshot: {error:?}"))?,
                        bond_positions: managed
                            .bond_positions
                            .iter()
                            .map(|(nullifier, position)| (*nullifier, *position))
                            .collect(),
                        bond_origin: managed.bond_origin.clone(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
        };
        serde_json::to_vec(&stored)
            .map_err(|error| format!("encode Names runtime checkpoint: {error}"))
    }

    pub(crate) fn load_checkpoint(
        network: WalletNetwork,
        config: NamesWalletConfig,
        bytes: &[u8],
    ) -> Result<Self, String> {
        let stored: StoredRuntimeCheckpoint = serde_json::from_slice(bytes)
            .map_err(|error| format!("decode Names runtime checkpoint: {error}"))?;
        if stored.format_version != STORE_FORMAT_VERSION {
            return Err(format!(
                "unsupported Names runtime checkpoint format {}",
                stored.format_version
            ));
        }
        let core_parameters = config.validated_core_parameters(network)?;
        let replay_configuration =
            CoreReplayConfiguration::new(config.activation_height, config.retention_blocks)
                .map_err(|error| format!("invalid Core replay configuration: {error:?}"))?;
        let core = CoreRuntime::load_snapshot(core_parameters, replay_configuration, &stored.core)
            .map_err(|error| format!("load Core runtime snapshot: {error:?}"))?;
        let parameters = config.parameters(network)?;
        let verifier = proof_verifier();
        let tree_size = u64::try_from(core.ironwood_frontier().size())
            .map_err(|_| "checkpoint Ironwood tree size exceeds u64".to_string())?;
        let mut resolvers = BTreeMap::new();
        for stored_resolver in stored.resolvers {
            let name = Name::parse(&stored_resolver.name).map_err(|error| {
                format!(
                    "invalid checkpoint Names label {:?}: {error:?}",
                    stored_resolver.name
                )
            })?;
            let canonical = name.as_str().to_owned();
            let resolver = ExactResolver::load_snapshot(
                parameters,
                config.network(),
                name,
                Arc::clone(&verifier),
                &stored_resolver.resolver,
            )
            .map_err(|error| format!("load exact Names snapshot: {error:?}"))?;
            let tip_matches = resolver.tip().map_or_else(
                || core.tip().height.checked_add(1) == Some(config.activation_height),
                |tip| tip.height == core.tip().height && tip.hash == core.tip().block_hash,
            );
            if !tip_matches {
                return Err("Core and exact Names checkpoint tips differ".into());
            }
            let mut bond_positions = BTreeMap::new();
            for (nullifier, position) in stored_resolver.bond_positions {
                FieldElement::from_bytes(nullifier)
                    .map_err(|_| "checkpoint contains a noncanonical nullifier".to_string())?;
                if u64::from(position) >= tree_size
                    || bond_positions.insert(nullifier, position).is_some()
                {
                    return Err("checkpoint contains an invalid managed bond position".into());
                }
            }
            if let Some(origin) = &stored_resolver.bond_origin {
                let (epoch, ua, action_index, action_nullifier) = match origin {
                    BondOrigin::Reveal {
                        epoch,
                        ua,
                        action_index,
                        action_nullifier,
                        ..
                    }
                    | BondOrigin::Refresh {
                        epoch,
                        ua,
                        action_index,
                        action_nullifier,
                        ..
                    } => (*epoch, ua, *action_index, *action_nullifier),
                };
                CanonicalUa::parse(config.network(), ua)
                    .map_err(|_| "checkpoint contains an invalid bond-origin UA".to_string())?;
                FieldElement::from_bytes(action_nullifier).map_err(|_| {
                    "checkpoint contains a noncanonical bond-origin nullifier".to_string()
                })?;
                let head = resolver
                    .resolve(core.tip().height)
                    .head
                    .ok_or_else(|| "checkpoint bond origin has no resolved head".to_string())?;
                if head.producer_epoch != epoch
                    || head.producer.action_index != action_index
                    || head.ua.as_str() != ua
                {
                    return Err("checkpoint bond origin does not match resolved head".into());
                }
            }
            if resolvers
                .insert(
                    canonical,
                    ManagedResolver {
                        resolver,
                        bond_positions,
                        bond_origin: stored_resolver.bond_origin,
                    },
                )
                .is_some()
            {
                return Err("checkpoint contains a duplicate managed name".into());
            }
        }
        Ok(Self {
            network,
            config,
            core,
            resolvers,
        })
    }

    pub(crate) fn tip_height(&self) -> u32 {
        self.core.tip().height
    }

    pub(crate) fn can_apply_start(&self, height: u32) -> bool {
        self.tip_height()
            .checked_add(1)
            .is_some_and(|expected| expected == height)
    }

    pub(crate) fn routes_for_height(&self, height: u32) -> Result<Vec<CoreRendezvous>, String> {
        let parameters = self.config.parameters(self.network)?;
        let mut routes = Vec::new();
        for name in self.resolvers.keys() {
            let name = Name::parse(name).expect("managed names are canonical");
            let name_id = name.id().expect("managed name ID is canonical");
            if !parameters.accepts_operation(name_id, height) {
                continue;
            }
            let route = NameRoute::derive(parameters.deployment_id, name_id)
                .map_err(|error| format!("derive name route: {error:?}"))?;
            routes.push(
                CoreRendezvous::try_new(&route.incoming_viewing_key(), &route.receiver())
                    .map_err(|error| format!("construct name rendezvous: {error:?}"))?,
            );
        }
        Ok(routes)
    }

    pub(crate) fn managed_resolutions(&self) -> Vec<ManagedResolution> {
        let height = self.tip_height();
        self.resolvers
            .iter()
            .map(|(name, managed)| {
                let resolution = managed.resolver.resolve(height);
                ManagedResolution {
                    name: name.clone(),
                    lifecycle: resolution.lifecycle,
                    marked_position: resolution.head.as_ref().and_then(|head| {
                        managed
                            .bond_positions
                            .get(&head.future_nf.to_bytes())
                            .copied()
                    }),
                    bond_origin: managed.bond_origin.clone(),
                    head: resolution.head,
                }
            })
            .collect()
    }

    fn status(&self) -> NamesWalletStatus {
        NamesWalletStatus {
            state: "ready".into(),
            message: "authenticated exact-name replay is synchronized".into(),
            configured: true,
            tip_height: u64::from(self.tip_height()),
            names_activation_height: u64::from(self.config.activation_height),
            oldest_rewind_height: u64::from(self.core.oldest_rewind_height()),
        }
    }

    /// Applies one compact block to this host fork, including exact dynamic
    /// route acquisition, and returns the corresponding wallet-tree delta.
    pub(crate) fn apply_compact_block<P, S>(
        &mut self,
        consensus: &P,
        compact: &CompactBlock,
        source: &mut S,
    ) -> Result<ManagedBlockDelta, String>
    where
        P: Parameters,
        S: FullTransactionSource,
        S::Error: std::fmt::Debug,
    {
        let height = u32::try_from(compact.height)
            .map_err(|_| "compact block height exceeds u32".to_string())?;
        let routes = self.routes_for_height(height)?;
        let context = apply_compact_block_with_additional_rendezvous(
            consensus,
            &mut self.core,
            compact,
            source,
            &routes,
        )
        .map_err(|error| format!("apply canonical Names block: {error:?}"))?;
        self.apply_authenticated_block(context.core())
    }

    pub(crate) async fn apply_compact_blocks(
        &mut self,
        client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
        blocks: Vec<CompactBlock>,
    ) -> Result<Vec<ManagedBlockDelta>, String> {
        let Some(first) = blocks.first() else {
            return Ok(Vec::new());
        };
        let first_height = u32::try_from(first.height)
            .map_err(|_| "Names batch height exceeds u32".to_string())?;
        if !self.can_apply_start(first_height) {
            return Err(format!(
                "Names host tip {} is not immediately before batch {first_height}",
                self.tip_height()
            ));
        }

        let mut txids = BTreeSet::new();
        for block in &blocks {
            let height = u32::try_from(block.height)
                .map_err(|_| "Names batch height exceeds u32".to_string())?;
            let routes = self.routes_for_height(height)?;
            for transaction in &block.vtx {
                if compact_tx_matches_any_route(transaction, self.core.rendezvous(), &routes)? {
                    txids.insert(
                        exact_32(&transaction.txid).ok_or_else(|| {
                            "compact transaction has a non-32-byte txid".to_string()
                        })?,
                    );
                }
            }
        }
        let mut full_transactions = BTreeMap::new();
        let mut acquired_bytes = 0usize;
        for txid in txids {
            let raw: RawTransaction = super::sync_engine::get_transaction(client, txid.to_vec())
                .await
                .map_err(|error| format!("get_transaction {}: {error}", hex::encode(txid)))?;
            if raw.data.len() > coppice_librustzcash::MAX_FULL_TRANSACTION_BYTES {
                return Err(format!(
                    "full transaction {} exceeds acquisition limit",
                    hex::encode(txid)
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
            full_transactions.insert(txid, raw.data);
        }
        let mut source = MapFullTransactionSource(full_transactions);
        let mut candidate = self.fork();
        let mut deltas = Vec::with_capacity(blocks.len());
        for block in &blocks {
            let network = candidate.network;
            deltas.push(candidate.apply_compact_block(&network, block, &mut source)?);
        }
        *self = candidate;
        Ok(deltas)
    }

    /// Applies one already Core-authenticated block to every managed exact
    /// resolver and returns the wallet-tree changes that must commit with the
    /// corresponding wallet scan.
    pub(crate) fn apply_authenticated_block(
        &mut self,
        context: &coppice::replay::CoreBlockContext,
    ) -> Result<ManagedBlockDelta, String> {
        let deployment = self.config.deployment(self.network)?;
        let network = self.config.network();
        let core_parameters = self.config.validated_core_parameters(self.network)?;
        let mut mark_positions = BTreeMap::new();
        let mut remove_marks = Vec::new();
        let mut accepted_commits = BTreeMap::new();

        for (canonical_name, managed) in &mut self.resolvers {
            let name = Name::parse(canonical_name).expect("managed names are canonical");
            let block =
                inspect_exact_name_block(context, &core_parameters, deployment, network, &name)
                    .map_err(|error| format!("inspect exact Names block: {error:?}"))?;

            for transaction in &block.transactions {
                for action in &transaction.actions {
                    if let Some(position) =
                        managed.bond_positions.remove(&action.nullifier.to_bytes())
                    {
                        remove_marks.push(position);
                    }
                }
            }

            managed
                .resolver
                .apply_block(&block)
                .map_err(|error| format!("apply exact Names block: {error:?}"))?;
            for transaction in &block.transactions {
                let Some(coppice_names::codec::Operation::Commit { commitment }) =
                    transaction.operation.as_ref()
                else {
                    continue;
                };
                let reference = coppice_names::protocol::CommitRef {
                    height: block.height,
                    tx_index: transaction.tx_index,
                    txid: transaction.txid,
                };
                if managed.resolver.pending_commit(&reference) == Some(*commitment) {
                    accepted_commits.insert(commitment.to_bytes(), reference);
                }
            }
            let resolution = managed.resolver.resolve(block.height);
            let Some(head) = resolution
                .head
                .filter(|head| head.producer.height == block.height)
            else {
                continue;
            };
            let position = authenticated_action_position(
                context,
                head.producer.tx_index,
                head.producer.action_index,
            )
            .map_err(|error| format!("derive managed Names action position: {error:?}"))?;
            managed
                .bond_positions
                .insert(head.future_nf.to_bytes(), position);
            let producer = block
                .transactions
                .iter()
                .find(|transaction| transaction.tx_index == head.producer.tx_index)
                .ok_or_else(|| "accepted Names producer transaction is absent".to_string())?;
            let action = producer
                .actions
                .get(
                    usize::try_from(head.producer.action_index)
                        .map_err(|_| "accepted Names action index exceeds usize".to_string())?,
                )
                .ok_or_else(|| "accepted Names producer action is absent".to_string())?;
            managed.bond_origin = match producer.operation.as_ref() {
                Some(coppice_names::codec::Operation::Reveal {
                    commit,
                    ua,
                    action_index,
                    ..
                }) => Some(BondOrigin::Reveal {
                    commit: *commit,
                    epoch: head.producer_epoch,
                    ua: ua.as_str().to_owned(),
                    action_index: *action_index,
                    action_nullifier: action.nullifier.to_bytes(),
                }),
                Some(coppice_names::codec::Operation::Refresh {
                    predecessor,
                    ua,
                    action_index,
                    ..
                }) => Some(BondOrigin::Refresh {
                    predecessor: *predecessor,
                    epoch: head.producer_epoch,
                    ua: ua.as_str().to_owned(),
                    action_index: *action_index,
                    action_nullifier: action.nullifier.to_bytes(),
                }),
                _ => return Err("accepted Names head has no state operation".into()),
            };
            mark_positions.insert(position, (head.producer.txid, head.producer.action_index));
        }

        remove_marks.sort_unstable();
        remove_marks.dedup();
        let mut positioned_leaves = Vec::new();
        for transaction in context.transactions() {
            for (action_index, commitment) in transaction
                .ironwood_effects()
                .commitments()
                .iter()
                .enumerate()
            {
                let action_index = u32::try_from(action_index)
                    .map_err(|_| "Ironwood action index exceeds u32".to_string())?;
                let position =
                    authenticated_action_position(context, transaction.tx_index(), action_index)
                        .map_err(|error| format!("derive block action position: {error:?}"))?;
                positioned_leaves.push(ManagedLeaf {
                    commitment: *commitment,
                    mark: mark_positions.contains_key(&position),
                });
            }
        }
        let block_start_position = context
            .transactions()
            .iter()
            .flat_map(|transaction| {
                (0..transaction.ironwood_effects().commitments().len()).map(move |index| {
                    (
                        transaction.tx_index(),
                        u32::try_from(index).unwrap_or(u32::MAX),
                    )
                })
            })
            .next()
            .map(|(tx_index, action_index)| {
                authenticated_action_position(context, tx_index, action_index)
                    .map_err(|error| format!("derive block start position: {error:?}"))
            })
            .transpose()?;
        if mark_positions.len() != positioned_leaves.iter().filter(|leaf| leaf.mark).count() {
            return Err("managed Names head is absent from the complete block action list".into());
        }
        Ok(ManagedBlockDelta {
            height: context.height(),
            block_start_position,
            leaves: positioned_leaves,
            remove_marks,
            accepted_commits: accepted_commits.into_iter().collect(),
        })
    }
}

fn host_from_stored(
    network: WalletNetwork,
    stored: &StoredNamesWallet,
) -> Result<Option<NamesWalletHost>, String> {
    if stored.config.network_code != network_code(network) {
        return Err("Names sidecar belongs to a different wallet network".into());
    }
    validate_deployed_config(network, &stored.config)?;
    let checkpoint = stored.checkpoint.as_deref().filter(|checkpoint| {
        stored.checkpoint_tag.is_some_and(|tag| {
            tag == checkpoint_cache_tag(&stored.config, &stored.tracked_names, checkpoint)
        })
    });
    let host = checkpoint
        .map(|bytes| NamesWalletHost::load_checkpoint(network, stored.config.clone(), bytes))
        .transpose()?;
    if let Some(host) = &host {
        let missing = stored
            .tracked_names
            .iter()
            .chain(
                stored
                    .registrations
                    .iter()
                    .map(|registration| &registration.name),
            )
            .any(|name| !host.resolvers.contains_key(name));
        if missing {
            return Err("Names checkpoint omits a tracked exact resolver".into());
        }
    }
    Ok(host)
}

pub(crate) fn configure(
    db_path: &str,
    network: WalletNetwork,
    retention_blocks: u32,
) -> Result<NamesWalletStatus, String> {
    let config = deployed_config(network, retention_blocks)?;
    let path = sidecar_path(db_path);
    with_sidecar_lock(&path, || {
        if let Some(existing) = read_stored(&path)? {
            if existing.config != config {
                return Err("cannot change Names deployment while wallet state exists".into());
            }
            if let Some(host) = host_from_stored(network, &existing)? {
                return Ok(host.status());
            }
            return Ok(needs_bootstrap_status(&config));
        }
        write_stored(
            &path,
            &StoredNamesWallet {
                format_version: STORE_FORMAT_VERSION,
                config: config.clone(),
                checkpoint: None,
                checkpoint_tag: None,
                registrations: Vec::new(),
                tracked_names: Vec::new(),
            },
        )?;
        Ok(needs_bootstrap_status(&config))
    })
}

fn needs_bootstrap_status(config: &NamesWalletConfig) -> NamesWalletStatus {
    NamesWalletStatus {
        state: "needs_bootstrap".into(),
        message: "authenticated replay is required before Names operations".into(),
        configured: true,
        tip_height: 0,
        names_activation_height: u64::from(config.activation_height),
        oldest_rewind_height: 0,
    }
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
    match host_from_stored(network, &stored) {
        Ok(Some(host)) => Ok(host.status()),
        Ok(None) => Ok(needs_bootstrap_status(&stored.config)),
        Err(error) => Ok(NamesWalletStatus {
            state: "corrupt".into(),
            message: error,
            configured: true,
            tip_height: 0,
            names_activation_height: u64::from(stored.config.activation_height),
            oldest_rewind_height: 0,
        }),
    }
}

pub(crate) fn is_configured(db_path: &str) -> Result<bool, String> {
    Ok(read_stored(&sidecar_path(db_path))?.is_some())
}

/// Whether ordinary wallet scanning must have the Names host available before
/// it can safely insert Ironwood leaves. Owned-name workflows may have a
/// hidden bond output in the next block, so scanning without the host could
/// prune the witness before the output is recognized and marked.
pub(crate) fn requires_managed_scanning(db_path: &str) -> Result<bool, String> {
    Ok(read_stored(&sidecar_path(db_path))?.is_some_and(|stored| !stored.registrations.is_empty()))
}

pub(crate) fn managed_activation_height(
    db_path: &str,
    network: WalletNetwork,
) -> Result<u32, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    validate_deployed_config(network, &stored.config)?;
    Ok(stored.config.activation_height)
}

pub(crate) fn configured_names_metadata(
    db_path: &str,
    network: WalletNetwork,
) -> Result<ConfiguredNamesMetadata, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let parameters = stored.config.parameters(network)?;
    let tip_height = host_from_stored(network, &stored)?.map_or(0, |host| host.tip_height());
    Ok(ConfiguredNamesMetadata {
        parameters,
        tip_height,
    })
}

pub(crate) fn lifecycle_context(
    db_path: &str,
    network: WalletNetwork,
) -> Result<NamesLifecycleContext, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let host = host_from_stored(network, &stored)?
        .ok_or_else(|| "Names must be bootstrapped before operations".to_string())?;
    Ok(NamesLifecycleContext {
        parameters: stored.config.parameters(network)?,
        deployment: stored.config.deployment(network)?,
        network: stored.config.network(),
        rendezvous_receiver: stored
            .config
            .rendezvous_receiver
            .clone()
            .try_into()
            .map_err(|bytes: Vec<u8>| {
                format!("Names rendezvous receiver has {} bytes", bytes.len())
            })?,
        tip_height: host.tip_height(),
    })
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
    let canonical = Name::parse(name)
        .map_err(|error| format!("invalid Names label: {error:?}"))?
        .as_str()
        .to_owned();
    Ok(registrations(db_path)?.into_iter().find(|registration| {
        registration.account_uuid == account_uuid && registration.name == canonical
    }))
}

pub(crate) fn store_registration(
    db_path: &str,
    registration: StoredRegistration,
) -> Result<(), String> {
    update_stored(db_path, |stored| {
        if stored.registrations.iter().any(|existing| {
            existing.account_uuid == registration.account_uuid && existing.name == registration.name
        }) {
            return Err("this account already manages that name".into());
        }
        if !stored
            .tracked_names
            .iter()
            .any(|name| name == &registration.name)
        {
            stored.tracked_names.push(registration.name.clone());
            stored.tracked_names.sort();
            stored.tracked_names.dedup();
            // The checkpoint cannot pretend to resolve a newly tracked name.
            stored.checkpoint = None;
            stored.checkpoint_tag = None;
        }
        stored.registrations.push(registration);
        Ok(())
    })
}

pub(crate) fn replace_registration(
    db_path: &str,
    registration: StoredRegistration,
) -> Result<(), String> {
    update_stored(db_path, |stored| {
        let existing = stored
            .registrations
            .iter_mut()
            .find(|existing| {
                existing.account_uuid == registration.account_uuid
                    && existing.name == registration.name
            })
            .ok_or_else(|| "Names registration workflow is unavailable".to_string())?;
        *existing = registration;
        Ok(())
    })
}

pub(crate) fn take_cancelled_registration(
    db_path: &str,
    send_flow_id: &str,
) -> Result<Option<StoredRegistration>, String> {
    let mut removed = None;
    update_stored(db_path, |stored| {
        if let Some(index) = stored.registrations.iter().position(|registration| {
            registration.send_flow_id.as_deref() == Some(send_flow_id)
                && registration.phase == "commit_proposed"
        }) {
            removed = Some(stored.registrations.remove(index));
        }
        Ok(())
    })?;
    Ok(removed)
}

pub(crate) fn take_registration_workflow(
    db_path: &str,
    account_uuid: &str,
    name: &str,
) -> Result<Option<StoredRegistration>, String> {
    let mut removed = None;
    update_stored(db_path, |stored| {
        if let Some(index) = stored.registrations.iter().position(|registration| {
            registration.account_uuid == account_uuid && registration.name == name
        }) {
            removed = Some(stored.registrations.remove(index));
        }
        Ok(())
    })?;
    Ok(removed)
}

pub(crate) fn record_commit_broadcast(
    db_path: &str,
    send_flow_id: &str,
    txid: [u8; 32],
) -> Result<(), String> {
    update_stored(db_path, |stored| {
        let registration = stored
            .registrations
            .iter_mut()
            .find(|registration| registration.send_flow_id.as_deref() == Some(send_flow_id))
            .ok_or_else(|| "Names COMMIT workflow is unavailable".to_string())?;
        registration.commit_txid = Some(txid);
        registration.phase = "commit_broadcast".into();
        Ok(())
    })
}

pub(crate) fn record_reveal_broadcast(
    db_path: &str,
    account_uuid: &str,
    name: &str,
    txid: [u8; 32],
) -> Result<(), String> {
    update_stored(db_path, |stored| {
        let registration = stored
            .registrations
            .iter_mut()
            .find(|registration| {
                registration.account_uuid == account_uuid && registration.name == name
            })
            .ok_or_else(|| "Names registration workflow is unavailable".to_string())?;
        registration.reveal_txid = Some(txid);
        registration.phase = "reveal_broadcast".into();
        Ok(())
    })
}

pub(crate) fn accepted_commit(
    db_path: &str,
    network: WalletNetwork,
    commitment: [u8; 32],
) -> Result<Option<coppice_names::protocol::CommitRef>, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let Some(registration) = stored
        .registrations
        .iter()
        .find(|registration| registration.commitment == commitment)
    else {
        return Ok(None);
    };
    let (Some(height), Some(tx_index), Some(txid)) = (
        registration.commit_height,
        registration.commit_tx_index,
        registration.commit_txid,
    ) else {
        return Ok(None);
    };
    let reference = coppice_names::protocol::CommitRef {
        height,
        tx_index,
        txid,
    };
    let Some(host) = host_from_stored(network, &stored)? else {
        return Ok(None);
    };
    let canonical = Name::parse(&registration.name)
        .map_err(|error| format!("invalid stored Names label: {error:?}"))?;
    Ok(host
        .resolvers
        .get(canonical.as_str())
        .and_then(|managed| managed.resolver.pending_commit(&reference))
        .filter(|accepted| accepted.to_bytes() == commitment)
        .map(|_| reference))
}

pub(crate) fn accepted_managed_resolution(
    db_path: &str,
    network: WalletNetwork,
    name: &str,
) -> Result<Option<ManagedResolution>, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let Some(host) = host_from_stored(network, &stored)? else {
        return Ok(None);
    };
    let canonical = Name::parse(name).map_err(|error| format!("invalid Names label: {error:?}"))?;
    Ok(host
        .managed_resolutions()
        .into_iter()
        .find(|resolution| resolution.name == canonical.as_str()))
}

pub(crate) fn managed_registrations(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<Vec<StoredRegistration>, String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    let resolutions = host_from_stored(network, &stored)?
        .map(|host| {
            host.managed_resolutions()
                .into_iter()
                .map(|resolution| (resolution.name.clone(), resolution))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    Ok(stored
        .registrations
        .into_iter()
        .filter(|registration| registration.account_uuid == account_uuid)
        .map(|mut registration| {
            if let Some(resolution) = resolutions.get(&registration.name) {
                registration.phase = lifecycle_label(resolution.lifecycle).into();
                if let Some(head) = &resolution.head {
                    registration.ua = head.ua.as_str().to_owned();
                }
            }
            registration
        })
        .collect())
}

pub(crate) fn load_for_sync(
    db_path: &str,
    network: WalletNetwork,
) -> Result<Option<NamesWalletHost>, String> {
    let Some(stored) = read_stored(&sidecar_path(db_path))? else {
        return Ok(None);
    };
    host_from_stored(network, &stored)
}

pub(crate) fn persist_for_sync(db_path: &str, host: &NamesWalletHost) -> Result<(), String> {
    persist_after_scan(db_path, host, &[])
}

pub(crate) fn persist_after_scan(
    db_path: &str,
    host: &NamesWalletHost,
    deltas: &[ManagedBlockDelta],
) -> Result<(), String> {
    let checkpoint = host.save_checkpoint()?;
    let tracked_names = host.resolvers.keys().cloned().collect::<Vec<_>>();
    let checkpoint_tag = checkpoint_cache_tag(&host.config, &tracked_names, &checkpoint);
    let commit_ttl_blocks = host.config.deployment(host.network)?.commit_ttl_blocks;
    update_stored(db_path, |stored| {
        if stored.config != host.config {
            return Err("Names configuration changed during synchronization".into());
        }
        stored.checkpoint = Some(checkpoint);
        stored.checkpoint_tag = Some(checkpoint_tag);
        stored.tracked_names = tracked_names;
        for (commitment, reference) in deltas
            .iter()
            .flat_map(|delta| delta.accepted_commits.iter())
        {
            if let Some(registration) = stored
                .registrations
                .iter_mut()
                .find(|registration| registration.commitment == *commitment)
            {
                registration.commit_height = Some(reference.height);
                registration.commit_tx_index = Some(reference.tx_index);
                registration.commit_txid = Some(reference.txid);
                if registration.reveal_txid.is_none() {
                    registration.phase = "commit_accepted".into();
                }
            }
        }
        for registration in &mut stored.registrations {
            if registration.reveal_txid.is_none() {
                if registration.commit_height.is_some_and(|height| {
                    height.saturating_add(commit_ttl_blocks) <= host.tip_height()
                }) {
                    registration.phase = "commit_expired".into();
                    continue;
                }
                let name = Name::parse(&registration.name)
                    .map_err(|error| format!("invalid stored Names label: {error:?}"))?;
                let window = host
                    .config
                    .parameters(host.network)?
                    .window(
                        name.id()
                            .map_err(|error| format!("derive stored name ID: {error:?}"))?,
                        registration.target_epoch,
                    )
                    .map_err(|error| format!("derive stored Names window: {error:?}"))?;
                if host.tip_height().saturating_add(1) >= window.end {
                    registration.phase = "window_missed".into();
                }
            }
        }
        Ok(())
    })
}

pub(crate) fn invalidate_after_reorg(
    db_path: &str,
    host: &mut Option<NamesWalletHost>,
    _rewind_height: u32,
) {
    *host = None;
    let _ = update_stored(db_path, |stored| {
        stored.checkpoint = None;
        stored.checkpoint_tag = None;
        Ok(())
    });
}

pub(crate) fn disable_after_error(db_path: &str, host: &mut Option<NamesWalletHost>) {
    invalidate_after_reorg(db_path, host, 0);
}

/// Ensures a custody-sensitive Names host is positioned immediately before a
/// wallet scan batch. A missing, unauthenticated, ahead-of-wallet, or
/// wrong-branch cache is rebuilt from the deployment activation checkpoint;
/// it is never accepted merely because its mutable sidecar claims a height.
pub(crate) async fn ensure_for_managed_scan(
    db_path: &str,
    network: WalletNetwork,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    host: &mut Option<NamesWalletHost>,
    target_height: u32,
) -> Result<(), String> {
    let stored = read_stored(&sidecar_path(db_path))?
        .ok_or_else(|| "Names is not configured".to_string())?;
    validate_deployed_config(network, &stored.config)?;

    let mut candidate = host
        .take()
        .or_else(|| host_from_stored(network, &stored).ok().flatten());
    if candidate
        .as_ref()
        .is_some_and(|candidate| candidate.tip_height() > target_height)
    {
        candidate = None;
    }
    if let Some(mut existing) = candidate {
        let forward = if existing.tip_height() < target_height {
            let start_height = existing.tip_height().saturating_add(1);
            replay_range(&mut existing, client, start_height, target_height).await
        } else {
            Ok(())
        };
        if forward.is_ok() {
            let canonical_hash =
                sync_engine::get_compact_block_hash(client, u64::from(target_height))
                    .await
                    .map_err(|error| error.to_string())?;
            if existing.core.tip().block_hash == canonical_hash.0 {
                persist_for_sync(db_path, &existing)?;
                *host = Some(existing);
                return Ok(());
            }
        }
    }

    let base_height = stored
        .config
        .activation_height
        .checked_sub(1)
        .ok_or_else(|| "Names activation has no parent height".to_string())?;
    if target_height < base_height {
        return Err(format!(
            "wallet scan predecessor {target_height} precedes Names activation parent {base_height}"
        ));
    }
    let checkpoint = activation_checkpoint(client, base_height).await?;
    let names = stored
        .tracked_names
        .iter()
        .cloned()
        .chain(
            stored
                .registrations
                .iter()
                .map(|registration| registration.name.clone()),
        )
        .collect::<BTreeSet<_>>();
    let mut rebuilt = NamesWalletHost::from_checkpoint(network, stored.config, checkpoint, names)?;
    replay_range(
        &mut rebuilt,
        client,
        base_height.saturating_add(1),
        target_height,
    )
    .await?;
    persist_for_sync(db_path, &rebuilt)?;
    *host = Some(rebuilt);
    Ok(())
}

pub(crate) async fn bootstrap(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
) -> Result<NamesWalletStatus, String> {
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
    let base_height = stored
        .config
        .activation_height
        .checked_sub(1)
        .ok_or_else(|| "Names activation has no parent height".to_string())?;
    if tip_height < base_height {
        return Err(format!(
            "lightwalletd tip {tip_height} precedes Names activation parent {base_height}"
        ));
    }
    let checkpoint = activation_checkpoint(&mut client, base_height).await?;
    let names = stored
        .tracked_names
        .iter()
        .cloned()
        .chain(
            stored
                .registrations
                .iter()
                .map(|registration| registration.name.clone()),
        )
        .collect::<BTreeSet<_>>();
    let mut host = NamesWalletHost::from_checkpoint(network, stored.config, checkpoint, names)?;
    replay_range(
        &mut host,
        &mut client,
        base_height.saturating_add(1),
        tip_height,
    )
    .await?;
    persist_for_sync(db_path, &host)?;
    Ok(host.status())
}

pub(crate) async fn resolve_name(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    name: &str,
) -> Result<NamesResolution, String> {
    let name = Name::parse(name).map_err(|error| format!("invalid Names label: {error:?}"))?;
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

    let mut scanned = 0u64;
    let existing_host = host_from_stored(network, &stored)?;
    let mut host = match existing_host {
        Some(host) if host.resolvers.contains_key(name.as_str()) => host,
        existing => {
            let base_height = stored
                .config
                .activation_height
                .checked_sub(1)
                .ok_or_else(|| "Names activation has no parent height".to_string())?;
            if tip_height < base_height {
                return Err(format!(
                    "lightwalletd tip {tip_height} precedes Names activation parent {base_height}"
                ));
            }
            let checkpoint = activation_checkpoint(&mut client, base_height).await?;
            let names = existing
                .into_iter()
                .flat_map(|host| host.resolvers.into_keys())
                .chain(stored.tracked_names.iter().cloned())
                .chain(std::iter::once(name.as_str().to_owned()))
                .collect::<BTreeSet<_>>();
            NamesWalletHost::from_checkpoint(network, stored.config.clone(), checkpoint, names)?
        }
    };
    if host.tip_height() < tip_height {
        let start = host.tip_height().saturating_add(1);
        scanned = u64::from(tip_height.saturating_sub(start).saturating_add(1));
        replay_range(&mut host, &mut client, start, tip_height).await?;
    }
    let managed = host
        .resolvers
        .get(name.as_str())
        .ok_or_else(|| "exact resolver disappeared".to_string())?;
    let resolution = managed.resolver.resolve(tip_height);
    let result = NamesResolution {
        status: lifecycle_label(resolution.lifecycle).into(),
        payment_address: resolution.ua.map(|ua| ua.as_str().to_owned()),
        lease_expiry: resolution
            .head
            .as_ref()
            .map(|head| u64::from(head.expiry_height)),
        terminal_height: resolution
            .head
            .as_ref()
            .and_then(|head| head.terminal_height)
            .map(u64::from),
        producer: resolution.head.map(|head| head.producer),
        tip_height: u64::from(tip_height),
        compact_blocks_scanned: scanned,
    };
    persist_for_sync(db_path, &host)?;
    Ok(result)
}

async fn replay_range(
    host: &mut NamesWalletHost,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    start: u32,
    end: u32,
) -> Result<(), String> {
    if start > end {
        return Ok(());
    }
    let mut next = start;
    while next <= end {
        let batch_end = next
            .saturating_add(ACQUISITION_BATCH_BLOCKS.saturating_sub(1))
            .min(end);
        let blocks = sync_engine::download_blocks_vec(
            client,
            BlockHeight::from_u32(next),
            BlockHeight::from_u32(batch_end),
            host.network,
        )
        .await
        .map_err(|error| format!("download Names blocks {next}..={batch_end}: {error}"))?;
        host.apply_compact_blocks(client, blocks).await?;
        next = batch_end
            .checked_add(1)
            .ok_or_else(|| "Names replay height overflow".to_string())?;
    }
    Ok(())
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

fn update_stored(
    db_path: &str,
    operation: impl FnOnce(&mut StoredNamesWallet) -> Result<(), String>,
) -> Result<(), String> {
    let path = sidecar_path(db_path);
    with_sidecar_lock(&path, || {
        let mut stored =
            read_stored(&path)?.ok_or_else(|| "Names is not configured".to_string())?;
        operation(&mut stored)?;
        write_stored(&path, &stored)
    })
}

fn lifecycle_label(lifecycle: Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Active => "active",
        Lifecycle::Cooldown => "cooldown",
        Lifecycle::Claimable => "claimable",
        Lifecycle::Missing => "missing",
    }
}

fn proof_verifier() -> Arc<OrchardProofVerifier> {
    static VERIFIER: OnceLock<Arc<OrchardProofVerifier>> = OnceLock::new();
    Arc::clone(VERIFIER.get_or_init(|| {
        let (_, verifier) = keygen();
        Arc::new(verifier)
    }))
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
    let bytes = hex::decode(hex_value).map_err(|error| format!("invalid {label} hex: {error}"))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("{label} must be {N} bytes, got {}", bytes.len()))
}

fn exact_32(bytes: &[u8]) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}

fn checkpoint_cache_key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0; 32];
        rand_10::rng().fill_bytes(&mut key);
        key
    })
}

fn checkpoint_cache_tag(
    config: &NamesWalletConfig,
    tracked_names: &[String],
    checkpoint: &[u8],
) -> [u8; 32] {
    let mut state = blake2b_simd::Params::new()
        .hash_length(32)
        .key(checkpoint_cache_key())
        .personal(b"CoppiceNamesCach")
        .to_state();
    state.update(&config.network_code.to_be_bytes());
    state.update(&config.activation_height.to_be_bytes());
    state.update(&config.retention_blocks.to_be_bytes());
    for field in [
        config.network_domain.as_slice(),
        config.rendezvous_ivk.as_slice(),
        config.rendezvous_receiver.as_slice(),
    ] {
        state.update(&(field.len() as u64).to_be_bytes());
        state.update(field);
    }
    state.update(&(tracked_names.len() as u64).to_be_bytes());
    for name in tracked_names {
        state.update(&(name.len() as u64).to_be_bytes());
        state.update(name.as_bytes());
    }
    state.update(&(checkpoint.len() as u64).to_be_bytes());
    state.update(checkpoint);
    state.finalize().as_bytes().try_into().expect("32-byte tag")
}

fn compact_tx_matches_any_route(
    transaction: &CompactTx,
    core: &CoreRendezvous,
    names: &[CoreRendezvous],
) -> Result<bool, String> {
    for (index, encoded) in transaction.ironwood_actions.iter().enumerate() {
        let action = CompactAction::try_from(encoded)
            .map_err(|_| format!("invalid compact Ironwood action {index}"))?;
        if core.compact_action_is_rendezvous(&action)
            || names
                .iter()
                .any(|route| route.compact_action_is_rendezvous(&action))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

struct MapFullTransactionSource(BTreeMap<[u8; 32], Vec<u8>>);

impl FullTransactionSource for MapFullTransactionSource {
    type Error = String;

    fn full_transaction(&mut self, txid: [u8; 32]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.0.get(&txid).cloned())
    }
}

fn sidecar_path(db_path: &str) -> PathBuf {
    PathBuf::from(format!("{db_path}{STORE_SUFFIX}"))
}

fn with_sidecar_lock<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let lock = {
        let mut locks = LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks
            .entry(path.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    operation()
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
    let bytes = fs::read(path)
        .map_err(|error| format!("read Names sidecar {}: {error}", path.display()))?;
    let stored: StoredNamesWallet =
        serde_json::from_slice(&bytes).map_err(|error| format!("decode Names sidecar: {error}"))?;
    if stored.format_version != STORE_FORMAT_VERSION {
        return Err(format!(
            "unsupported Names sidecar format {}",
            stored.format_version
        ));
    }
    Ok(Some(stored))
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
mod tests {
    use super::*;
    use coppice::replay::IronwoodFrontier;

    fn config() -> NamesWalletConfig {
        deployed_config(WalletNetwork::Regtest, 64).unwrap()
    }

    #[test]
    fn empty_multi_name_checkpoint_round_trips_at_one_branch() {
        let host = NamesWalletHost::from_checkpoint(
            WalletNetwork::Regtest,
            config(),
            CoreReplayActivationCheckpoint {
                height: 1,
                block_hash: [7; 32],
                ironwood_frontier: IronwoodFrontier::empty(),
                ironwood_tree_size: 0,
            },
            ["alice".to_string(), "bob".to_string()],
        )
        .unwrap();
        let restored = NamesWalletHost::load_checkpoint(
            WalletNetwork::Regtest,
            config(),
            &host.save_checkpoint().unwrap(),
        )
        .unwrap();
        assert_eq!(restored.tip_height(), 1);
        assert_eq!(restored.managed_resolutions().len(), 2);
        assert!(restored
            .managed_resolutions()
            .iter()
            .all(|resolution| resolution.lifecycle == Lifecycle::Missing));

        let mut fork = restored.fork();
        fork.resolvers.remove("alice");
        assert_eq!(fork.managed_resolutions().len(), 1);
        assert_eq!(restored.managed_resolutions().len(), 2);
    }

    #[test]
    fn persisted_checkpoint_is_only_a_process_authenticated_cache() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("wallet.db");
        let db_path = db_path.to_str().unwrap();
        configure(db_path, WalletNetwork::Regtest, 64).unwrap();
        let host = NamesWalletHost::from_checkpoint(
            WalletNetwork::Regtest,
            config(),
            CoreReplayActivationCheckpoint {
                height: 1,
                block_hash: [7; 32],
                ironwood_frontier: IronwoodFrontier::empty(),
                ironwood_tree_size: 0,
            },
            ["alice".to_string()],
        )
        .unwrap();
        persist_for_sync(db_path, &host).unwrap();
        assert!(load_for_sync(db_path, WalletNetwork::Regtest)
            .unwrap()
            .is_some());

        let path = sidecar_path(db_path);
        let mut stored = read_stored(&path).unwrap().unwrap();
        stored.checkpoint.as_mut().unwrap()[0] ^= 1;
        write_stored(&path, &stored).unwrap();
        assert!(load_for_sync(db_path, WalletNetwork::Regtest)
            .unwrap()
            .is_none());

        persist_for_sync(db_path, &host).unwrap();
        let mut stored = read_stored(&path).unwrap().unwrap();
        stored.checkpoint_tag = None;
        write_stored(&path, &stored).unwrap();
        assert!(load_for_sync(db_path, WalletNetwork::Regtest)
            .unwrap()
            .is_none());
    }

    #[test]
    fn sidecar_cannot_redefine_the_compiled_deployment() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("wallet.db");
        let db_path = db_path.to_str().unwrap();
        configure(db_path, WalletNetwork::Regtest, 64).unwrap();

        let path = sidecar_path(db_path);
        let mut stored = read_stored(&path).unwrap().unwrap();
        stored.config.network_domain = b"attacker-selected-domain".to_vec();
        write_stored(&path, &stored).unwrap();

        assert!(matches!(
            load_for_sync(db_path, WalletNetwork::Regtest),
            Err(error) if error.contains("deployment identity does not match")
        ));
    }

    #[test]
    fn sidecar_checkpoint_preserves_newer_workflow_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("wallet.db");
        let db_path = db_path.to_str().unwrap();
        let status = configure(db_path, WalletNetwork::Regtest, 64).unwrap();
        assert_eq!(status.state, "needs_bootstrap");
        assert!(!requires_managed_scanning(db_path).unwrap());
        let host = NamesWalletHost::from_checkpoint(
            WalletNetwork::Regtest,
            config(),
            CoreReplayActivationCheckpoint {
                height: 1,
                block_hash: [7; 32],
                ironwood_frontier: IronwoodFrontier::empty(),
                ironwood_tree_size: 0,
            },
            ["alice".to_string()],
        )
        .unwrap();
        store_registration(
            db_path,
            StoredRegistration {
                account_uuid: "account".into(),
                name: "alice".into(),
                ua: "uregtest1invalid-for-storage-only".into(),
                commitment: [2; 32],
                target_epoch: 1,
                send_flow_id: None,
                bond_txid: None,
                bond_output_index: None,
                commit_height: None,
                commit_tx_index: None,
                phase: "awaiting_bond".into(),
                commit_txid: None,
                reveal_txid: None,
            },
        )
        .unwrap();
        assert!(requires_managed_scanning(db_path).unwrap());
        persist_for_sync(db_path, &host).unwrap();
        assert_eq!(registrations(db_path).unwrap()[0].phase, "awaiting_bond");
        assert_eq!(
            load_for_sync(db_path, WalletNetwork::Regtest)
                .unwrap()
                .unwrap()
                .tip_height(),
            1
        );
        store_registration(
            db_path,
            StoredRegistration {
                account_uuid: "account".into(),
                name: "bob".into(),
                ua: "uregtest1invalid-for-storage-only".into(),
                commitment: [3; 32],
                target_epoch: 1,
                send_flow_id: None,
                bond_txid: None,
                bond_output_index: None,
                commit_height: None,
                commit_tx_index: None,
                phase: "awaiting_bond".into(),
                commit_txid: None,
                reveal_txid: None,
            },
        )
        .unwrap();
        assert!(load_for_sync(db_path, WalletNetwork::Regtest)
            .unwrap()
            .is_none());
    }

    /// Opt-in smoke for the real local Zakura/Zaino deployment. Lifecycle
    /// proving is covered separately; this exercises the committed wallet
    /// host's gRPC activation checkpoint, exact replay, and cache reuse.
    #[tokio::test]
    #[ignore = "requires a live Zakura/Zaino regtest endpoint"]
    async fn live_zaino_bootstrap_and_missing_resolution() {
        let lightwalletd_url = std::env::var("COPPICE_NAMES_TEST_LIGHTWALLETD")
            .unwrap_or_else(|_| "http://127.0.0.1:9067".into());
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("wallet.sqlite");
        let db_path = db_path.to_str().unwrap();

        let configured = configure(db_path, WalletNetwork::Regtest, 128).unwrap();
        assert_eq!(configured.state, "needs_bootstrap");

        let first = resolve_name(
            db_path,
            &lightwalletd_url,
            WalletNetwork::Regtest,
            "coppice-wallet-smoke",
        )
        .await
        .unwrap();
        assert_eq!(first.status, "missing");
        assert!(first.tip_height >= u64::from(REGTEST_ACTIVATION_HEIGHT));
        assert!(first.compact_blocks_scanned > 0);

        let ready = bootstrap(db_path, &lightwalletd_url, WalletNetwork::Regtest)
            .await
            .unwrap();
        assert_eq!(ready.state, "ready");
        assert_eq!(ready.tip_height, first.tip_height);

        let cached = resolve_name(
            db_path,
            &lightwalletd_url,
            WalletNetwork::Regtest,
            "coppice-wallet-smoke",
        )
        .await
        .unwrap();
        assert_eq!(cached.status, "missing");
        assert_eq!(cached.tip_height, first.tip_height);
        assert_eq!(cached.compact_blocks_scanned, 0);
    }
}
