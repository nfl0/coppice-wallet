//! Flutter-facing Coppice Names v1 lifecycle and resolution API.
//!
//! These entrypoints deliberately keep deployment parameters explicit.  A
//! wallet can ship one configuration profile, but the Rust host never
//! silently substitutes a test identity or a trusted Names snapshot.

use crate::wallet::{coppice, keys, network::WalletNetwork};
use flutter_rust_bridge::frb;
use zeroize::Zeroizing;

pub struct ApiNamesWalletStatus {
    pub state: String,
    pub message: String,
    pub configured: bool,
    pub tip_height: u64,
    pub names_activation_height: u64,
    pub oldest_rewind_height: u64,
}

pub struct ApiNamesResolution {
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

/// Wallet-owned denomination readiness for starting a registration.
pub struct ApiNamesBondStatus {
    pub state: String,
    pub required_zatoshi: u64,
    pub exact_note_count: u32,
    pub spendable_ironwood_zatoshi: u64,
}

/// Ordinary-wallet proposal that carries a Names COMMIT. The caller sends it
/// through the same review/prove/broadcast path as every other wallet send.
pub struct ApiNamesCommitProposal {
    pub proposal_id: u64,
    pub fee_zatoshi: u64,
    pub commitment: Vec<u8>,
}

/// Durable registration intent state. A draft survives the ordinary wallet
/// self-transfer used to form an exact bond denomination.
pub struct ApiNamesRegistrationDraft {
    pub phase: String,
}

pub struct ApiManagedName {
    pub name: String,
    pub payment_address: Option<String>,
    pub phase: String,
    pub commitment: Vec<u8>,
    /// Present only after canonical replay has authenticated this exact
    /// COMMIT. These are workflow display values, not independent evidence.
    pub commit_height: Option<u64>,
    pub commit_expiry_height: Option<u64>,
    pub commit_blocks_remaining: Option<u64>,
    pub next_reveal_height: Option<u64>,
    pub reveal_blocks_until: Option<u64>,
    pub reveal_ready: bool,
}

impl From<coppice::NamesWalletStatus> for ApiNamesWalletStatus {
    fn from(status: coppice::NamesWalletStatus) -> Self {
        Self {
            state: status.state,
            message: status.message,
            configured: status.configured,
            tip_height: status.tip_height,
            names_activation_height: status.names_activation_height,
            oldest_rewind_height: status.oldest_rewind_height,
        }
    }
}

impl From<coppice::NamesResolution> for ApiNamesResolution {
    fn from(result: coppice::NamesResolution) -> Self {
        Self {
            status: result.status,
            record: result.record,
            payment_address: result.payment_address,
            sequence: result.sequence,
            lease_expiry: result.lease_expiry,
            terminal_height: result.terminal_height,
            state_commitment: result.state_commitment,
            tip_height: result.tip_height,
            candidate_block_probes: result.candidate_block_probes,
            tail_blocks_scanned: result.tail_blocks_scanned,
            lineage_block_probes: result.lineage_block_probes,
            predecessor_chain_steps: result.predecessor_chain_steps,
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[frb(sync)]
pub fn configure_names_v1(
    db_path: String,
    network: String,
    runtime_activation_height: u64,
    names_activation_height: u64,
    epoch_size: u64,
    commit_ttl_blocks: u64,
    refresh_deadline_blocks: u64,
    lease_duration_blocks: u64,
    grace_period_blocks: u64,
    reuse_delay_blocks: u64,
    max_record_bytes: u64,
    minimum_bond_zatoshis: u64,
    retention_blocks: u64,
    network_domain: String,
    rendezvous_ivk_hex: String,
    rendezvous_receiver_hex: String,
) -> Result<ApiNamesWalletStatus, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let to_u32 = |value: u64, field: &str| {
        u32::try_from(value).map_err(|_| format!("{field} exceeds supported u32 range"))
    };
    let status = coppice::configure(
        &db_path,
        network,
        to_u32(runtime_activation_height, "runtime activation height")?,
        to_u32(names_activation_height, "Names activation height")?,
        to_u32(epoch_size, "epoch size")?,
        to_u32(commit_ttl_blocks, "commit TTL")?,
        to_u32(refresh_deadline_blocks, "refresh deadline")?,
        to_u32(lease_duration_blocks, "lease duration")?,
        to_u32(grace_period_blocks, "grace period")?,
        to_u32(reuse_delay_blocks, "reuse delay")?,
        usize::try_from(max_record_bytes)
            .map_err(|_| "max record bytes exceeds supported usize range".to_string())?,
        minimum_bond_zatoshis,
        to_u32(retention_blocks, "retention blocks")?,
        network_domain,
        rendezvous_ivk_hex,
        rendezvous_receiver_hex,
    )?;
    Ok(status.into())
}

#[frb(sync)]
pub fn get_names_v1_status(
    db_path: String,
    network: String,
) -> Result<ApiNamesWalletStatus, String> {
    let network =
        WalletNetwork::from_str(&network).ok_or_else(|| format!("Unknown network: {network}"))?;
    Ok(coppice::status(&db_path, network)?.into())
}

/// Checks the selected account for an exact, spendable and unreserved one-ZEC
/// Ironwood note. A `needs_preparation` result tells the UI to ask the wallet
/// send engine for an ordinary one-ZEC self-transfer before COMMIT.
#[frb(sync)]
pub fn get_names_v1_bond_status(
    db_path: String,
    network: String,
    account_uuid: String,
) -> Result<ApiNamesBondStatus, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let status = crate::wallet::names_lifecycle::bond_status(&db_path, network, &account_uuid)?;
    Ok(ApiNamesBondStatus {
        state: status.state,
        required_zatoshi: status.required_zatoshi,
        exact_note_count: status.exact_note_count,
        spendable_ironwood_zatoshi: status.spendable_ironwood_zatoshi,
    })
}

#[frb(sync)]
pub fn get_managed_names_v1(
    db_path: String,
    network: String,
    account_uuid: String,
) -> Result<Vec<ApiManagedName>, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let context = coppice::lifecycle_context(&db_path, network)?;
    let current_tip = context.tip_height;
    let commit_ttl_blocks = context.params.commit_ttl_blocks;
    let payment_network = context.payment_network;
    coppice::managed_registrations(&db_path, network, &account_uuid)?
        .into_iter()
        .map(|registration| {
            let payment_address =
                coppice_names::v1::PaymentRecord::decode(&registration.record, payment_network)
                    .ok()
                    .map(|record| record.address().to_string());
            let commit_expiry_height = registration
                .commit_height
                .map(|height| height.saturating_add(commit_ttl_blocks));
            let commit_blocks_remaining =
                commit_expiry_height.map(|expiry| expiry.saturating_sub(current_tip));
            let construction_height = current_tip.saturating_add(1);
            let next_reveal_height = if registration.phase == "commit_accepted" {
                coppice_names::v1::state::name_id(&registration.name)
                    .ok()
                    .and_then(|name_id| {
                        coppice_names::v1::schedule::next_anchor_height(
                            name_id,
                            construction_height,
                            context.params,
                        )
                    })
            } else {
                None
            };
            Ok(ApiManagedName {
                name: registration.name,
                payment_address,
                phase: registration.phase,
                commitment: registration.commitment.to_vec(),
                commit_height: registration.commit_height.map(u64::from),
                commit_expiry_height: commit_expiry_height.map(u64::from),
                commit_blocks_remaining: commit_blocks_remaining.map(u64::from),
                next_reveal_height: next_reveal_height.map(u64::from),
                reveal_blocks_until: next_reveal_height
                    .map(|height| height.saturating_sub(construction_height))
                    .map(u64::from),
                reveal_ready: next_reveal_height == Some(construction_height),
            })
        })
        .collect()
}

/// Persist a registration intent before the wallet prepares an exact bond.
/// If an eligible note already exists it is immediately reserved; otherwise
/// sync will reserve the self-transfer output as soon as it is confirmed.
pub fn prepare_names_v1_registration_draft(
    db_path: String,
    network: String,
    account_uuid: String,
    name: String,
    payment_address: String,
    mnemonic_bytes: Vec<u8>,
) -> Result<ApiNamesRegistrationDraft, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let mnemonic_bytes = Zeroizing::new(mnemonic_bytes);
    let seed = keys::mnemonic_bytes_to_seed(mnemonic_bytes.as_slice())?;
    drop(mnemonic_bytes);
    crate::wallet::names_lifecycle::prepare_registration_draft(
        &db_path,
        network,
        &account_uuid,
        &name,
        &payment_address,
        seed,
    )?;
    let canonical_name = name.trim().to_ascii_lowercase();
    let registration = coppice::registration(&db_path, &account_uuid, &canonical_name)?
        .ok_or_else(|| "prepared Names registration draft disappeared".to_string())?;
    Ok(ApiNamesRegistrationDraft {
        phase: registration.phase,
    })
}

/// Discards an uncompleted wallet-local workflow after the user explicitly
/// abandons it. Canonical COMMIT/REVEAL state is never altered here.
#[frb(sync)]
pub fn discard_names_v1_registration_workflow(
    db_path: String,
    network: String,
    account_uuid: String,
    name: String,
) -> Result<(), String> {
    let network = keys::parse_network(&network)?;
    crate::wallet::names_lifecycle::discard_registration_workflow(
        &db_path,
        network,
        &account_uuid,
        &name,
    )
}

/// Reserve the exact one-ZEC registration bond and create a COMMIT carrier
/// proposal. This intentionally does not broadcast: the established wallet
/// review and credential flow remains the sole transaction-execution path.
pub fn begin_names_v1_registration(
    db_path: String,
    network: String,
    account_uuid: String,
    send_flow_id: String,
    name: String,
    payment_address: String,
    mnemonic_bytes: Vec<u8>,
) -> Result<ApiNamesCommitProposal, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let mnemonic_bytes = Zeroizing::new(mnemonic_bytes);
    let seed = keys::mnemonic_bytes_to_seed(mnemonic_bytes.as_slice())?;
    drop(mnemonic_bytes);
    let proposal = crate::wallet::names_lifecycle::begin_registration(
        &db_path,
        network,
        &account_uuid,
        &send_flow_id,
        &name,
        &payment_address,
        seed,
    )?;
    Ok(ApiNamesCommitProposal {
        proposal_id: proposal.proposal_id,
        fee_zatoshi: proposal.fee_zatoshi,
        commitment: proposal.commitment.to_vec(),
    })
}

/// Proves and broadcasts REVEAL after the runtime has authenticated the exact
/// accepted COMMIT and the canonical schedule reaches this name's anchor.
pub fn reveal_names_v1_registration(
    db_path: String,
    lightwalletd_url: String,
    network: String,
    account_uuid: String,
    name: String,
    mnemonic_bytes: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let mnemonic_bytes = Zeroizing::new(mnemonic_bytes);
    let seed = keys::mnemonic_bytes_to_seed(mnemonic_bytes.as_slice())?;
    drop(mnemonic_bytes);
    let runtime = tokio::runtime::Runtime::new().map_err(|error| format!("tokio: {error}"))?;
    let txid = runtime.block_on(crate::wallet::names_lifecycle::reveal_registration(
        &db_path,
        &lightwalletd_url,
        network,
        &account_uuid,
        &name,
        seed,
    ))?;
    Ok(txid.to_vec())
}

/// Proves and broadcasts one canonical current-head transition.
pub fn manage_name_v1(
    db_path: String,
    lightwalletd_url: String,
    network: String,
    account_uuid: String,
    name: String,
    action: String,
    payment_address: Option<String>,
    mnemonic_bytes: Vec<u8>,
) -> Result<Vec<u8>, String> {
    use crate::wallet::names_lifecycle::NamesTransitionKind;

    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let kind = match action.as_str() {
        "update" => NamesTransitionKind::Update(
            payment_address.ok_or_else(|| "UPDATE requires a payment address".to_string())?,
        ),
        "renew" => NamesTransitionKind::Renew,
        "release" => NamesTransitionKind::Release,
        _ => return Err(format!("unknown Names management action: {action}")),
    };
    let mnemonic_bytes = Zeroizing::new(mnemonic_bytes);
    let seed = keys::mnemonic_bytes_to_seed(mnemonic_bytes.as_slice())?;
    drop(mnemonic_bytes);
    let runtime = tokio::runtime::Runtime::new().map_err(|error| format!("tokio: {error}"))?;
    let txid = runtime.block_on(crate::wallet::names_lifecycle::execute_transition(
        &db_path,
        &lightwalletd_url,
        network,
        &account_uuid,
        &name,
        kind,
        seed,
    ))?;
    Ok(txid.to_vec())
}

pub async fn bootstrap_names_v1(
    db_path: String,
    lightwalletd_url: String,
    network: String,
) -> Result<ApiNamesWalletStatus, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    Ok(coppice::bootstrap(&db_path, &lightwalletd_url, network)
        .await?
        .into())
}

pub async fn resolve_name_v1(
    db_path: String,
    lightwalletd_url: String,
    network: String,
    name: String,
) -> Result<ApiNamesResolution, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    Ok(
        coppice::resolve_name(&db_path, &lightwalletd_url, network, &name)
            .await?
            .into(),
    )
}
