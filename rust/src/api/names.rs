//! Flutter-facing replacement Coppice Names lifecycle and resolution API.

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
    pub payment_address: Option<String>,
    pub lease_expiry: Option<u64>,
    pub terminal_height: Option<u64>,
    pub producer_txid: Option<Vec<u8>>,
    pub producer_height: Option<u64>,
    pub producer_tx_index: Option<u64>,
    pub producer_action_index: Option<u64>,
    pub tip_height: u64,
    pub compact_blocks_scanned: u64,
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

/// Reviewed REVEAL proposal backed by an atomically consumed Rust capability.
pub struct ApiNamesRevealProposal {
    pub proposal_id: u64,
    pub fee_zatoshi: u64,
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
    /// Half-open inclusion-height window in which this draft's COMMIT can
    /// mature for its already-bound REVEAL height without expiring.
    pub commit_window_start: u64,
    pub commit_window_end: u64,
    pub commit_blocks_until: u64,
    pub commit_window_open: bool,
    /// The deterministic half-open REVEAL window selected by this workflow.
    pub reveal_window_start: u64,
    pub reveal_window_end: u64,
    pub reveal_blocks_until: u64,
    pub reveal_window_open: bool,
    /// Next deterministic REFRESH window for an active accepted head.
    pub refresh_window_start: Option<u64>,
    pub refresh_window_end: Option<u64>,
    pub refresh_blocks_until: Option<u64>,
    pub refresh_window_open: bool,
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
        let producer = result.producer;
        Self {
            status: result.status,
            payment_address: result.payment_address,
            lease_expiry: result.lease_expiry,
            terminal_height: result.terminal_height,
            producer_txid: producer.as_ref().map(|producer| producer.txid.to_vec()),
            producer_height: producer.as_ref().map(|producer| u64::from(producer.height)),
            producer_tx_index: producer
                .as_ref()
                .map(|producer| u64::from(producer.tx_index)),
            producer_action_index: producer
                .as_ref()
                .map(|producer| u64::from(producer.action_index)),
            tip_height: result.tip_height,
            compact_blocks_scanned: result.compact_blocks_scanned,
        }
    }
}

#[frb(sync)]
pub fn configure_names(
    db_path: String,
    network: String,
    retention_blocks: u64,
) -> Result<ApiNamesWalletStatus, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let to_u32 = |value: u64, field: &str| {
        u32::try_from(value).map_err(|_| format!("{field} exceeds supported u32 range"))
    };
    let status = coppice::configure(
        &db_path,
        network,
        to_u32(retention_blocks, "retention blocks")?,
    )?;
    Ok(status.into())
}

#[frb(sync)]
pub fn get_names_status(db_path: String, network: String) -> Result<ApiNamesWalletStatus, String> {
    let network =
        WalletNetwork::from_str(&network).ok_or_else(|| format!("Unknown network: {network}"))?;
    Ok(coppice::status(&db_path, network)?.into())
}

/// Checks the selected account for an exact, spendable and unreserved one-ZEC
/// Ironwood note. A `needs_preparation` result tells the UI to ask the wallet
/// send engine for an ordinary one-ZEC self-transfer before COMMIT.
#[frb(sync)]
pub fn get_names_bond_status(
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
pub fn get_managed_names(
    db_path: String,
    network: String,
    account_uuid: String,
) -> Result<Vec<ApiManagedName>, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    // Managed rows include durable wallet-local workflow phases such as
    // `awaiting_bond` and `bond_reserved`. They must remain readable after a
    // replay failure has removed the derived host and requested rebootstrap.
    let metadata = coppice::configured_names_metadata(&db_path, network)?;
    let current_tip = metadata.tip_height;
    let commit_ttl_blocks = metadata.parameters.commit_ttl_blocks;
    coppice::managed_registrations(&db_path, network, &account_uuid)?
        .into_iter()
        .map(|managed| {
            let registration = managed.workflow;
            let name = coppice_names::protocol::Name::parse(&registration.name)
                .map_err(|error| format!("invalid stored Names label: {error:?}"))?;
            let name_id = name
                .id()
                .map_err(|error| format!("derive stored name ID: {error:?}"))?;
            let window = metadata
                .parameters
                .window(name_id, registration.target_epoch)
                .map_err(|error| format!("derive stored Names window: {error:?}"))?;
            let next_height = current_tip.saturating_add(1);
            let refresh_window = managed
                .resolution
                .and_then(|resolution| resolution.head)
                .map(|head| {
                    let predecessor_epoch = metadata
                        .parameters
                        .epoch(head.producer.height)
                        .map_err(|error| format!("derive predecessor epoch: {error:?}"))?;
                    let current_epoch = metadata
                        .parameters
                        .epoch(next_height)
                        .map_err(|error| format!("derive current Names epoch: {error:?}"))?;
                    let mut epoch = current_epoch.max(predecessor_epoch.saturating_add(1));
                    let mut window = metadata
                        .parameters
                        .window(name_id, epoch)
                        .map_err(|error| format!("derive REFRESH window: {error:?}"))?;
                    if next_height >= window.end {
                        epoch = epoch
                            .checked_add(1)
                            .ok_or_else(|| "Names epoch overflow".to_string())?;
                        window = metadata
                            .parameters
                            .window(name_id, epoch)
                            .map_err(|error| format!("derive next REFRESH window: {error:?}"))?;
                    }
                    Ok::<_, String>(window)
                })
                .transpose()?;
            let payment_address = Some(registration.ua.clone());
            let commit_expiry_height = registration
                .commit_height
                .map(|height| height.saturating_add(commit_ttl_blocks));
            let commit_blocks_remaining =
                commit_expiry_height.map(|expiry| expiry.saturating_sub(current_tip));
            let commit_window_start = registration
                .target_reveal_height
                .saturating_sub(commit_ttl_blocks.saturating_sub(1));
            let commit_window_end = registration
                .target_reveal_height
                .saturating_sub(metadata.parameters.commit_maturity_blocks)
                .saturating_add(1);
            Ok(ApiManagedName {
                name: registration.name,
                payment_address,
                phase: registration.phase,
                commitment: registration.commitment.to_vec(),
                commit_height: registration.commit_height.map(u64::from),
                commit_expiry_height: commit_expiry_height.map(u64::from),
                commit_blocks_remaining: commit_blocks_remaining.map(u64::from),
                commit_window_start: u64::from(commit_window_start),
                commit_window_end: u64::from(commit_window_end),
                commit_blocks_until: u64::from(commit_window_start.saturating_sub(next_height)),
                commit_window_open: commit_window_start <= next_height
                    && next_height < commit_window_end,
                reveal_window_start: u64::from(window.start),
                reveal_window_end: u64::from(window.end),
                reveal_blocks_until: u64::from(window.start.saturating_sub(next_height)),
                reveal_window_open: window.contains(next_height),
                refresh_window_start: refresh_window.map(|window| u64::from(window.start)),
                refresh_window_end: refresh_window.map(|window| u64::from(window.end)),
                refresh_blocks_until: refresh_window
                    .map(|window| u64::from(window.start.saturating_sub(next_height))),
                refresh_window_open: refresh_window
                    .is_some_and(|window| window.contains(next_height)),
            })
        })
        .collect()
}

/// Persist a registration intent before the wallet prepares an exact bond.
/// If an eligible note already exists it is immediately reserved; otherwise
/// sync will reserve the self-transfer output as soon as it is confirmed.
pub fn prepare_names_registration_draft(
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
pub fn discard_names_registration_workflow(
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
pub fn begin_names_registration(
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

/// Builds and signs a Names REVEAL after the canonical COMMIT is accepted and
/// while its protocol-defined TTL is still live. Execution is later routed
/// through the ordinary proposal review/confirmation entrypoint.
pub fn begin_names_reveal(
    db_path: String,
    lightwalletd_url: String,
    network: String,
    account_uuid: String,
    send_flow_id: String,
    name: String,
    mnemonic_bytes: Vec<u8>,
) -> Result<ApiNamesRevealProposal, String> {
    let network = keys::parse_network(&network)?;
    keys::ensure_db_migrated_once(&db_path, network)?;
    let mnemonic_bytes = Zeroizing::new(mnemonic_bytes);
    let seed = keys::mnemonic_bytes_to_seed(mnemonic_bytes.as_slice())?;
    drop(mnemonic_bytes);
    let runtime = tokio::runtime::Runtime::new().map_err(|error| format!("tokio: {error}"))?;
    let proposal = runtime.block_on(crate::wallet::names_lifecycle::begin_reviewed_reveal(
        &db_path,
        &lightwalletd_url,
        network,
        &account_uuid,
        &name,
        &send_flow_id,
        seed,
    ))?;
    Ok(ApiNamesRevealProposal {
        proposal_id: proposal.proposal_id,
        fee_zatoshi: proposal.fee_zatoshi,
    })
}

/// Proves and broadcasts REVEAL after the runtime has authenticated the exact
/// accepted COMMIT and the protocol-defined COMMIT TTL is still live.
pub fn reveal_names_registration(
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
pub fn manage_name(
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

pub async fn bootstrap_names(
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

pub async fn resolve_name(
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
