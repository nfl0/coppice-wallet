//! Flutter-facing Coppice Names v1 lifecycle and resolution API.
//!
//! These entrypoints deliberately keep deployment parameters explicit.  A
//! wallet can ship one configuration profile, but the Rust host never
//! silently substitutes a test identity or a trusted Names snapshot.

use crate::wallet::{coppice, keys, network::WalletNetwork};
use flutter_rust_bridge::frb;

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
