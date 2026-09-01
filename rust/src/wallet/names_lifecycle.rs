//! Wallet-owned Coppice Names lifecycle integration.
//!
//! The deterministic Names crate requests an exact one-ZEC Ironwood bond.
//! This module inspects the selected wallet account and reports whether that
//! note exists or whether the wallet must prepare it with an ordinary
//! self-transfer. Names never performs wallet-wide coin selection itself.

use coppice_names::v1::{OrchardV1ProofProver, PaymentRecord, RegistrationIntent};
use coppice_names_wallet::{
    builder::{
        build_names_v1_bundle, build_names_v1_pczt, extract_names_v1_transaction,
        finalize_names_v1_pczt_io, install_names_v1_ironwood_witnesses,
        prove_names_v1_ironwood_pczt, sign_names_v1_ironwood_pczt, ChangeOutput, FundingSpend,
        NamesV1IronwoodSigningKey, NamesV1IronwoodWitness, NamesV1PcztPlan, NamesV1SigningPlan,
        NamesV1WitnessPlan,
    },
    classify_bond_inventory,
    operation::{
        plan_state_operation, planned_state_operation_shape_and_fee, prepare_commit,
        prepare_release, prepare_renew, prepare_reveal, prepare_update, CarrierPlan,
        OperationFunding, RevealInputs, SuccessorTransport, TransitionInputs,
    },
    BondInventoryDecision, REQUIRED_BOND_ZATOSHIS,
};
use orchard::{
    circuit::state_note_binding::spend_auth_owner_key_bytes,
    keys::{FullViewingKey, Scope, SpendAuthorizingKey},
    note::ExtractedNoteCommitment,
    value::NoteValue,
};
use rand::RngCore;
use secrecy::{ExposeSecret, SecretVec};
use std::sync::{Arc, Mutex};
use zcash_client_backend::{
    data_api::{
        locking::{LockFilter, OutputLockStore},
        wallet::{input_selection::LockedInputPolicy, TargetHeight},
        Account as _, InputSource, WalletCommitmentTrees, WalletRead,
    },
    wallet::{LockOwner, OutputRef},
};
use zcash_client_sqlite::error::SqliteClientError;
use zcash_keys::{address::UnifiedAddress, keys::UnifiedSpendingKey};
use zcash_primitives::transaction::TxId;
use zcash_protocol::{
    consensus::{BlockHeight, BranchId, Parameters},
    PoolType, ShieldedPool,
};

use super::{
    coppice::{self, StoredRegistration},
    db::with_wallet_db_write_lock,
    keys::parse_account_uuid,
    network::WalletNetwork,
    sync::{self, open_wallet_db, open_wallet_db_for_read},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamesBondStatus {
    pub state: String,
    pub required_zatoshi: u64,
    pub exact_note_count: u32,
    pub spendable_ironwood_zatoshi: u64,
}

pub(crate) fn bond_status(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<NamesBondStatus, String> {
    let account_id = parse_account_uuid(account_uuid)?;
    let db = open_wallet_db_for_read(db_path, network)?;
    let target_height = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .map_err(|error| format!("read Names bond target height: {error}"))?
        .ok_or_else(|| "wallet has no synchronized target height".to_string())?
        .0;
    let target = TargetHeight::from(target_height);
    let historical_height = u32::from(target_height).saturating_sub(1);
    let candidates = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(historical_height),
        )
        .map_err(|error| format!("read Ironwood bond inventory: {error}"))?;

    let mut values = Vec::new();
    for candidate in candidates {
        let spendable = db
            .get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                target,
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .map_err(|error| format!("classify Ironwood bond candidate: {error}"))?;
        if let Some(note) = spendable {
            values.push(note.note().value().into_u64());
        }
    }
    values.sort_unstable();
    let exact_note_count = u32::try_from(
        values
            .iter()
            .filter(|value| **value == REQUIRED_BOND_ZATOSHIS)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let spendable_ironwood_zatoshi = values.iter().copied().fold(0u64, u64::saturating_add);
    let state = match classify_bond_inventory(values) {
        BondInventoryDecision::Ready => "ready",
        BondInventoryDecision::PrepareExactNote => "needs_preparation",
        BondInventoryDecision::InsufficientFunds => "insufficient_funds",
    };
    Ok(NamesBondStatus {
        state: state.to_string(),
        required_zatoshi: REQUIRED_BOND_ZATOSHIS,
        exact_note_count,
        spendable_ironwood_zatoshi,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamesCommitProposal {
    pub proposal_id: u64,
    pub fee_zatoshi: u64,
    pub commitment: [u8; 32],
}

fn canonical_registration_name(name: &str) -> Result<String, String> {
    let canonical_name = name.trim().to_ascii_lowercase();
    if canonical_name.is_empty() || canonical_name.contains('.') {
        return Err("enter the name label only; .zec is added by the wallet".to_string());
    }
    coppice_names::v1::state::name_id(&canonical_name).map_err(|_| {
        "Names labels use 1-63 lowercase letters, digits, or hyphens, with no leading or trailing hyphen"
            .to_string()
    })?;
    Ok(canonical_name)
}

fn registration_payment_record(
    existing: Option<&StoredRegistration>,
    payment_network: coppice_names::v1::PaymentNetwork,
    payment_address: &str,
) -> Result<Vec<u8>, String> {
    let requested = PaymentRecord::new(payment_network, payment_address)
        .map_err(|error| format!("invalid Names payment record: {error:?}"))?;
    let Some(existing) = existing else {
        return Ok(requested.encode());
    };
    let stored = PaymentRecord::decode(&existing.record, payment_network)
        .map_err(|error| format!("stored Names payment record is invalid: {error:?}"))?;
    if stored != requested {
        return Err(
            "this Names draft is bound to a different payment address; discard the workflow and register again to change it"
                .to_string(),
        );
    }
    Ok(existing.record.clone())
}

/// Records a user-approved registration intent before a denomination split.
/// The intent is durable, so sync can reserve the resulting exact one-ZEC
/// note immediately after the self-transfer confirms.
pub(crate) fn prepare_registration_draft(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    payment_address: &str,
    seed: SecretVec<u8>,
) -> Result<(), String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    let canonical_name = canonical_registration_name(name)?;
    if coppice::registration(db_path, account_uuid, &canonical_name)?.is_some() {
        return Err("this wallet account already has a registration workflow for that name".into());
    }
    let record = PaymentRecord::new(context.payment_network, payment_address)
        .map_err(|error| format!("invalid Names payment record: {error:?}"))?
        .encode();
    let account_id = parse_account_uuid(account_uuid)?;
    let db = open_wallet_db(db_path, network)?;
    let account = db
        .get_account(account_id)
        .map_err(|error| format!("read Names account: {error}"))?
        .ok_or_else(|| "Names account not found".to_string())?;
    let zip32_index = account
        .source()
        .key_derivation()
        .ok_or_else(|| "Names registration requires a software-derived account".to_string())?
        .account_index();
    let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), zip32_index)
        .map_err(|error| format!("derive Names spending key: {error:?}"))?;
    let ask = SpendAuthorizingKey::from(usk.orchard());
    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let intent = RegistrationIntent {
        name: canonical_name.clone(),
        owner_pk: spend_auth_owner_key_bytes(&ask),
        record: record.clone(),
        secret: registration_secret(seed.expose_secret(), account_uuid, &canonical_name, nonce),
    };
    drop(seed);
    let core_runtime_id = coppice::lifecycle_context(db_path, network)?.core_runtime_id;
    let commitment = prepare_commit(&intent, core_runtime_id)
        .map_err(|error| format!("prepare Names COMMIT: {error:#}"))?
        .commitment();
    coppice::store_registration(
        db_path,
        StoredRegistration {
            account_uuid: account_uuid.to_string(),
            name: canonical_name,
            record,
            nonce,
            commitment,
            send_flow_id: None,
            bond_txid: None,
            bond_output_index: None,
            commit_height: None,
            phase: "awaiting_bond".to_string(),
            commit_txid: None,
            reveal_txid: None,
        },
    )?;
    reserve_pending_bonds(db_path, network)
}

/// Reserves exact one-ZEC notes for durable user-approved registration drafts.
/// It runs after wallet sync, so a self-transfer cannot be raced by an
/// ordinary send between confirmation and the next Names screen visit.
/// Serialized on the wallet write lock: a UI-triggered reservation must queue
/// behind an in-flight sync batch instead of failing on `database is locked`.
pub(crate) fn reserve_pending_bonds(db_path: &str, network: WalletNetwork) -> Result<(), String> {
    with_wallet_db_write_lock("names.reserve_pending_bonds", || {
        reserve_pending_bonds_locked(db_path, network)
    })
}

fn reserve_pending_bonds_locked(db_path: &str, network: WalletNetwork) -> Result<(), String> {
    // Bond reservation is wallet custody state, not derived Names replay.
    // Keep it progressing when a replay checkpoint has been discarded and
    // the user is waiting for an exact self-transfer to become spendable.
    let metadata = coppice::configured_names_metadata(db_path, network)?;
    let pending = coppice::registrations(db_path)?;
    let mut db = open_wallet_db(db_path, network)?;
    let target_height = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .map_err(|error| format!("read Names bond target height: {error}"))?
        .ok_or_else(|| "wallet has no synchronized target height".to_string())?
        .0;
    for mut registration in pending
        .iter()
        .filter(|registration| registration.phase == "awaiting_bond")
        .cloned()
    {
        let account_id = parse_account_uuid(&registration.account_uuid)?;
        let candidates = db
            .get_unspent_ironwood_notes_at_historical_height(
                account_id,
                BlockHeight::from_u32(u32::from(target_height).saturating_sub(1)),
            )
            .map_err(|error| format!("read pending Names bond candidates: {error}"))?;
        let mut selected = None;
        for candidate in candidates {
            let spendable = db
                .get_spendable_note(
                    candidate.txid(),
                    ShieldedPool::Ironwood,
                    u32::from(candidate.output_index()),
                    TargetHeight::from(target_height),
                    LockFilter::Policy(&LockedInputPolicy::Exclude),
                )
                .map_err(|error| format!("classify pending Names bond: {error}"))?;
            if spendable
                .is_some_and(|note| note.note().value().into_u64() == REQUIRED_BOND_ZATOSHIS)
            {
                selected = Some(candidate);
                break;
            }
        }
        let Some(note) = selected else { continue };
        let output = OutputRef::new(
            *note.txid(),
            PoolType::Shielded(ShieldedPool::Ironwood),
            u32::from(note.output_index()),
        );
        let expiry = BlockHeight::from_u32(
            u32::from(target_height)
                .saturating_add(metadata.params.commit_ttl_blocks)
                .saturating_add(2),
        );
        db.lock_outputs(&[output], LockOwner::new(registration.commitment), expiry)
            .map_err(|error| format!("reserve prepared Names bond: {error:?}"))?;
        registration.bond_txid = Some((*note.txid()).into());
        registration.bond_output_index = Some(u32::from(note.output_index()));
        registration.phase = "bond_reserved".to_string();
        coppice::replace_registration(db_path, registration)?;
    }
    // A reserved bond's lock is height-bounded and was stamped when the note
    // was first reserved. If the user pauses before COMMIT, the lock can
    // expire while the workflow still needs the exact note, letting an
    // ordinary send consume it. Refresh the lock on every sync with the same
    // deterministic owner so `bond_reserved` stays protected.
    for registration in pending
        .iter()
        .filter(|registration| registration.phase == "bond_reserved")
    {
        let (Some(txid), Some(output_index)) =
            (registration.bond_txid, registration.bond_output_index)
        else {
            continue;
        };
        let output = OutputRef::new(
            TxId::from_bytes(txid),
            PoolType::Shielded(ShieldedPool::Ironwood),
            output_index,
        );
        let expiry = BlockHeight::from_u32(
            u32::from(target_height)
                .saturating_add(metadata.params.commit_ttl_blocks)
                .saturating_add(2),
        );
        db.lock_outputs(&[output], LockOwner::new(registration.commitment), expiry)
            .map_err(|error| format!("refresh reserved Names bond lock: {error:?}"))?;
    }
    Ok(())
}

fn registration_secret(seed: &[u8], account_uuid: &str, name: &str, nonce: [u8; 32]) -> [u8; 32] {
    let mut state = blake2b_simd::Params::new()
        .hash_length(32)
        .key(seed)
        .personal(b"CoppiceN1Wallet")
        .to_state();
    state.update(account_uuid.as_bytes());
    state.update(name.as_bytes());
    state.update(&nonce);
    state
        .finalize()
        .as_bytes()
        .try_into()
        .expect("32-byte registration secret")
}

/// Reserves an exact one-ZEC note and creates the ordinary wallet proposal
/// carrying the hidden COMMIT. The reserved note is excluded from all normal
/// wallet sends while the COMMIT/REVEAL workflow is pending.
pub(crate) fn begin_registration(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    send_flow_id: &str,
    name: &str,
    payment_address: &str,
    seed: SecretVec<u8>,
) -> Result<NamesCommitProposal, String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    // The application wallet policy deliberately prepares one exact ZEC,
    // while the proof parameter is a *minimum*. A deployment with a smaller
    // minimum (including the one-zatoshi regtest qualification profile)
    // accepts that one-ZEC state note; only a deployment requiring more than
    // the wallet policy can reject this flow.
    if context.params.minimum_bond_zatoshis > REQUIRED_BOND_ZATOSHIS {
        return Err(format!(
            "wallet supports an exact one-ZEC Names bond, but this deployment requires at least {} zatoshi",
            context.params.minimum_bond_zatoshis
        ));
    }
    let canonical_name = canonical_registration_name(name)?;

    let account_id = parse_account_uuid(account_uuid)?;
    let mut db = open_wallet_db(db_path, network)?;
    let account = db
        .get_account(account_id)
        .map_err(|error| format!("read Names account: {error}"))?
        .ok_or_else(|| "Names account not found".to_string())?;
    let zip32_index = account
        .source()
        .key_derivation()
        .ok_or_else(|| "Names registration requires a software-derived account".to_string())?
        .account_index();
    let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), zip32_index)
        .map_err(|error| format!("derive Names spending key: {error:?}"))?;
    let ask = SpendAuthorizingKey::from(usk.orchard());
    let existing = coppice::registration(db_path, account_uuid, &canonical_name)?;
    if let Some(registration) = &existing {
        if registration.phase != "awaiting_bond" && registration.phase != "bond_reserved" {
            return Err("this Names registration is already in progress or complete".to_string());
        }
    }
    // A draft's canonical payment record is immutable once stored. Validate
    // both sides and fail closed if the durable record cannot be decoded.
    let record =
        registration_payment_record(existing.as_ref(), context.payment_network, payment_address)?;
    let nonce = existing
        .as_ref()
        .map(|registration| registration.nonce)
        .unwrap_or_else(|| {
            let mut nonce = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut nonce);
            nonce
        });
    let intent = RegistrationIntent {
        name: canonical_name.clone(),
        owner_pk: spend_auth_owner_key_bytes(&ask),
        record: record.clone(),
        secret: registration_secret(seed.expose_secret(), account_uuid, &canonical_name, nonce),
    };
    drop(seed);
    let prepared = prepare_commit(&intent, context.core_runtime_id)
        .map_err(|error| format!("prepare Names COMMIT: {error:#}"))?;

    let target_height = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .map_err(|error| format!("read Names target height: {error}"))?
        .ok_or_else(|| "wallet must sync before registering a name".to_string())?
        .0;
    let historical_height = u32::from(target_height).saturating_sub(1);
    let candidates = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(historical_height),
        )
        .map_err(|error| format!("read Names bond candidates: {error}"))?;
    let mut exact = None;
    for candidate in candidates {
        let spendable = db
            .get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                TargetHeight::from(target_height),
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .map_err(|error| format!("classify exact Names bond: {error}"))?;
        if spendable.is_some_and(|note| note.note().value().into_u64() == REQUIRED_BOND_ZATOSHIS) {
            exact = Some(candidate);
            break;
        }
    }
    let exact =
        match (existing, exact) {
            (Some(registration), _) if registration.phase == "bond_reserved" => {
                let txid = registration.bond_txid.ok_or_else(|| {
                    "reserved Names bond is missing its transaction reference".to_string()
                })?;
                let output_index = registration.bond_output_index.ok_or_else(|| {
                    "reserved Names bond is missing its output reference".to_string()
                })?;
                db.get_unspent_ironwood_notes_at_historical_height(
                    account_id,
                    BlockHeight::from_u32(historical_height),
                )
                .map_err(|error| format!("read reserved Names bond: {error}"))?
                .into_iter()
                .find(|note| {
                    <[u8; 32]>::from(*note.txid()) == txid
                        && u32::from(note.output_index()) == output_index
                })
                .ok_or_else(|| "reserved Names bond is no longer unspent".to_string())?
            }
            (_, Some(exact)) => exact,
            _ => return Err(
                "Names requires an exact, confirmed one-ZEC Ironwood note; prepare that note first"
                    .to_string(),
            ),
        };
    let bond_ref = OutputRef::new(
        *exact.txid(),
        PoolType::Shielded(ShieldedPool::Ironwood),
        u32::from(exact.output_index()),
    );
    let lock_expiry = u32::from(target_height)
        .checked_add(context.params.commit_ttl_blocks)
        .and_then(|height| height.checked_add(2))
        .map(BlockHeight::from_u32)
        .ok_or_else(|| "Names bond lock expiry overflow".to_string())?;
    let lock_owner = LockOwner::new(prepared.commitment());
    with_wallet_db_write_lock("names.reserve_commit_bond", || {
        db.lock_outputs(&[bond_ref], lock_owner, lock_expiry)
            .map_err(|error| format!("reserve one-ZEC Names bond: {error:?}"))
    })?;
    drop(db);

    let receiver = Option::<orchard::Address>::from(orchard::Address::from_raw_address_bytes(
        &context.rendezvous_receiver,
    ))
    .ok_or_else(|| "configured Names rendezvous receiver is invalid".to_string())?;
    let ua = UnifiedAddress::from_receivers(Some(receiver), None, None)
        .ok_or_else(|| "construct Names rendezvous address".to_string())?
        .to_zcash_address(network.network_type())
        .to_string();
    let [frame] = prepared.frames() else {
        return Err(format!(
            "Names COMMIT must produce exactly one carrier frame, got {}",
            prepared.frames().len()
        ));
    };
    let proposal = match sync::propose_send_with_raw_memo(
        db_path,
        network,
        account_uuid,
        send_flow_id,
        &ua,
        1,
        Some(frame),
    ) {
        Ok(proposal) => proposal,
        Err(error) => {
            with_wallet_db_write_lock("names.release_commit_bond", || {
                let mut db = open_wallet_db(db_path, network)?;
                let _ = db.unlock_output(&bond_ref, lock_owner);
                Ok::<(), String>(())
            })?;
            return Err(error);
        }
    };
    let updated_registration = StoredRegistration {
        account_uuid: account_uuid.to_string(),
        name: canonical_name,
        record: intent.record.clone(),
        nonce,
        commitment: prepared.commitment(),
        send_flow_id: Some(send_flow_id.to_string()),
        bond_txid: Some((*exact.txid()).into()),
        bond_output_index: Some(u32::from(exact.output_index())),
        commit_height: None,
        phase: "commit_proposed".to_string(),
        commit_txid: None,
        reveal_txid: None,
    };
    let persisted =
        if coppice::registration(db_path, account_uuid, &updated_registration.name)?.is_some() {
            coppice::replace_registration(db_path, updated_registration)
        } else {
            coppice::store_registration(db_path, updated_registration)
        };
    if let Err(error) = persisted {
        let _ = sync::discard_proposal(proposal.proposal_id, send_flow_id);
        with_wallet_db_write_lock("names.release_commit_bond", || {
            let mut db = open_wallet_db(db_path, network)?;
            let _ = db.unlock_output(&bond_ref, lock_owner);
            Ok::<(), String>(())
        })?;
        return Err(error);
    }
    Ok(NamesCommitProposal {
        proposal_id: proposal.proposal_id,
        fee_zatoshi: proposal.fee_zatoshi,
        commitment: prepared.commitment(),
    })
}

/// Removes a cancelled pre-broadcast registration workflow and releases its
/// exact bond reservation. This is called by the generic proposal discard
/// path, so closing the ordinary send review cannot strand a Names bond.
pub(crate) fn cancel_registration_proposal(
    db_path: &str,
    network: WalletNetwork,
    send_flow_id: &str,
) -> Result<(), String> {
    let Some(registration) = coppice::take_cancelled_registration(db_path, send_flow_id)? else {
        return Ok(());
    };
    let (Some(txid), Some(output_index)) = (registration.bond_txid, registration.bond_output_index)
    else {
        return Ok(());
    };
    let output = OutputRef::new(
        TxId::from_bytes(txid),
        PoolType::Shielded(ShieldedPool::Ironwood),
        output_index,
    );
    with_wallet_db_write_lock("names.release_cancelled_bond", || {
        let mut db = open_wallet_db(db_path, network)?;
        db.unlock_output(&output, LockOwner::new(registration.commitment))
            .map(|_| ())
            .map_err(|error| format!("release cancelled Names bond: {error:?}"))
    })
}

/// Discards a locally abandoned pre-REVEAL workflow. This never changes
/// canonical Names history; it only lets the wallet start a new COMMIT after
/// an expired carrier or an explicitly abandoned proposal.
pub(crate) fn discard_registration_workflow(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
) -> Result<(), String> {
    let canonical_name = name.trim().to_ascii_lowercase();
    let Some(registration) = coppice::registration(db_path, account_uuid, &canonical_name)? else {
        return Ok(());
    };
    if !matches!(
        registration.phase.as_str(),
        "awaiting_bond"
            | "bond_reserved"
            | "commit_proposed"
            | "commit_broadcast"
            | "commit_expired"
    ) {
        return Err("only an uncompleted Names registration workflow can be discarded".to_string());
    }
    let registration = coppice::take_registration_workflow(db_path, account_uuid, &canonical_name)?
        .ok_or_else(|| "Names registration workflow disappeared".to_string())?;
    // A carrier may have been broadcast even if a stale UI record still says
    // `commit_proposed`. Never force-unlock its bond on that untrusted local
    // phase alone: the wallet's normal height-bounded lock will expire, while
    // canonical Names replay remains authoritative for acceptance/expiry.
    let may_force_unlock = matches!(
        registration.phase.as_str(),
        "awaiting_bond" | "bond_reserved" | "commit_expired"
    );
    if may_force_unlock {
        if let (Some(txid), Some(output_index)) =
            (registration.bond_txid, registration.bond_output_index)
        {
            let output = OutputRef::new(
                TxId::from_bytes(txid),
                PoolType::Shielded(ShieldedPool::Ironwood),
                output_index,
            );
            with_wallet_db_write_lock("names.release_discarded_bond", || {
                let mut db = open_wallet_db(db_path, network)?;
                let _ = db.unlock_output(&output, LockOwner::new(registration.commitment));
                Ok::<(), String>(())
            })?;
        }
    }
    Ok(())
}

pub(crate) struct NamesRevealTransaction {
    pub raw: Vec<u8>,
    pub txid: [u8; 32],
    pub name: String,
    pub account_uuid: String,
    pub db_path: String,
    pub network: WalletNetwork,
    pub valid_from_height: u32,
    pub expiry_height: u32,
    pub fee_zatoshi: u64,
    fee_reservation: Option<NamesFeeReservation>,
}

pub(crate) struct NamesRevealProposal {
    pub proposal_id: u64,
    pub fee_zatoshi: u64,
}

struct NamesFeeReservation {
    db_path: String,
    network: WalletNetwork,
    output: Option<OutputRef>,
    owner: LockOwner,
}

impl NamesFeeReservation {
    fn disarm(&mut self) {
        self.output = None;
    }
}

impl Drop for NamesFeeReservation {
    fn drop(&mut self) {
        let Some(output) = self.output.as_ref() else {
            return;
        };
        with_wallet_db_write_lock("names.release_failed_fee_note", || {
            if let Ok(mut db) = open_wallet_db(&self.db_path, self.network) {
                let _ = db.unlock_output(output, self.owner);
            }
        });
    }
}

fn operation_seed(seed: &[u8], commitment: [u8; 32], label: &[u8]) -> [u8; 32] {
    let mut state = blake2b_simd::Params::new()
        .hash_length(32)
        .key(seed)
        .personal(b"CoppiceN1State_")
        .to_state();
    state.update(&commitment);
    state.update(label);
    state
        .finalize()
        .as_bytes()
        .try_into()
        .expect("32-byte state seed")
}

/// Builds and authorizes REVEAL only after replay has authenticated the exact
/// canonical COMMIT producer position. The returned transaction is ready for
/// the wallet's lightwalletd broadcast path.
pub(crate) fn build_reveal(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    seed: SecretVec<u8>,
) -> Result<NamesRevealTransaction, String> {
    let registration = coppice::registrations(db_path)?
        .into_iter()
        .find(|registration| {
            registration.account_uuid == account_uuid
                && registration.name == name.trim().trim_end_matches(".zec").to_ascii_lowercase()
        })
        .ok_or_else(|| "this wallet has no pending registration for that name".to_string())?;
    if registration.phase == "reveal_broadcast" && registration.reveal_txid.is_some() {
        return Err("REVEAL is already broadcast and awaits confirmation".to_string());
    }
    let commit =
        coppice::accepted_commit(db_path, network, registration.commitment)?.ok_or_else(|| {
            "the exact COMMIT is not yet accepted in canonical Names history".to_string()
        })?;
    let context = coppice::lifecycle_context(db_path, network)?;
    let construction_height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names construction height overflow".to_string())?;
    let expiry_height = commit
        .position
        .height
        .checked_add(context.params.commit_ttl_blocks)
        .ok_or_else(|| "Names COMMIT lifetime overflow".to_string())?;

    let account_id = parse_account_uuid(account_uuid)?;
    let mut db = open_wallet_db(db_path, network)?;
    let account = db
        .get_account(account_id)
        .map_err(|error| format!("read Names account: {error}"))?
        .ok_or_else(|| "Names account not found".to_string())?;
    let zip32_index = account
        .source()
        .key_derivation()
        .ok_or_else(|| "Names registration requires a software-derived account".to_string())?
        .account_index();
    let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), zip32_index)
        .map_err(|error| format!("derive Names spending key: {error:?}"))?;
    let fvk = FullViewingKey::from(usk.orchard());
    let ask = SpendAuthorizingKey::from(usk.orchard());
    let intent = RegistrationIntent {
        name: registration.name.clone(),
        owner_pk: spend_auth_owner_key_bytes(&ask),
        record: registration.record.clone(),
        secret: registration_secret(
            seed.expose_secret(),
            account_uuid,
            &registration.name,
            registration.nonce,
        ),
    };
    let historical_height = context.tip_height;
    let candidates = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(historical_height),
        )
        .map_err(|error| format!("read Names operation notes: {error}"))?;
    let mut registration_note = None;
    let mut funding_note = None;
    let mut funding_value = 0u64;
    for candidate in candidates {
        let candidate_txid: [u8; 32] = (*candidate.txid()).into();
        if registration.bond_txid == Some(candidate_txid)
            && registration.bond_output_index == Some(u32::from(candidate.output_index()))
        {
            registration_note = Some(candidate);
            continue;
        }
        // One funding note must cover the carrier outputs plus the fee on its
        // own, so the largest spendable candidate is the only choice that can
        // possibly be adequate. Selecting the first spendable note instead
        // could fail even when a later note has enough value.
        let spendable = db
            .get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                TargetHeight::from(BlockHeight::from_u32(construction_height)),
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .map_err(|error| format!("classify Names fee note: {error}"))?;
        if spendable.is_some() {
            let value = candidate.note().value().inner();
            if value > funding_value {
                funding_value = value;
                funding_note = Some(candidate);
            }
        }
    }
    let registration_note = registration_note
        .ok_or_else(|| "reserved one-ZEC Names bond note is unavailable".to_string())?;
    let funding_note = funding_note.ok_or_else(|| {
        "wallet needs a separate confirmed Ironwood note for Names fees".to_string()
    })?;
    let registration_nf = registration_note.note().nullifier(&fvk).to_bytes();
    let funding_nf = funding_note.note().nullifier(&fvk).to_bytes();
    let preparation = prepare_reveal(
        RevealInputs {
            intent,
            commit,
            replacement_predecessor: None,
            registration_note: *registration_note.note(),
            scope: registration_note.spending_key_scope(),
            fvk: fvk.clone(),
            ask: ask.clone(),
            designated_action_index: 4,
            operation_height: construction_height,
            successor_seed: operation_seed(
                seed.expose_secret(),
                registration.commitment,
                b"reveal",
            ),
        },
        context.params,
    )
    .map_err(|error| format!("prepare Names REVEAL: {error:#}"))?;
    let funding_ref = OutputRef::new(
        *funding_note.txid(),
        PoolType::Shielded(ShieldedPool::Ironwood),
        u32::from(funding_note.output_index()),
    );
    let funding_lock_owner = LockOwner::new(preparation.statement().commitment);
    with_wallet_db_write_lock("names.reserve_reveal_fee_note", || {
        db.lock_outputs(
            &[funding_ref],
            funding_lock_owner,
            BlockHeight::from_u32(expiry_height),
        )
        .map_err(|error| format!("reserve Names REVEAL fee note: {error:?}"))
    })?;
    let fee_reservation = NamesFeeReservation {
        db_path: db_path.to_string(),
        network,
        output: Some(funding_ref),
        owner: funding_lock_owner,
    };
    // Every step after the fee-note reservation can still fail (proof,
    // planning, witnesses, signing, broadcast). Release the reservation so a
    // failed attempt never strands the note until lock expiry. Expensive proof
    // generation remains outside the wallet write lock; only commitment-tree
    // access is serialized with sync.
    let authorized = (|| -> Result<NamesRevealTransaction, String> {
        drop(seed);
        let proof = OrchardV1ProofProver::new()
            .prove_genesis(
                preparation.statement(),
                preparation.witness().clone(),
                rand_10::rng(),
            )
            .map_err(|error| format!("prove Names REVEAL: {error:?}"))?;
        let operation = preparation
            .finalize(proof, context.core_runtime_id)
            .map_err(|error| format!("finalize Names REVEAL: {error:#}"))?;

        let (shape, fee) = planned_state_operation_shape_and_fee(&network, &operation, 1, 1)
            .map_err(|error| format!("plan Names REVEAL fee: {error:#}"))?;
        let carrier_total = u64::try_from(operation.frames().len())
            .map_err(|_| "Names carrier count exceeds u64".to_string())?;
        let change_value = funding_note
            .note()
            .value()
            .inner()
            .checked_sub(carrier_total)
            .and_then(|value| value.checked_sub(fee.into_u64()))
            .ok_or_else(|| "separate Ironwood note cannot cover Names fee".to_string())?;
        let recipient = Option::<orchard::Address>::from(orchard::Address::from_raw_address_bytes(
            &context.rendezvous_receiver,
        ))
        .ok_or_else(|| "configured Names rendezvous receiver is invalid".to_string())?;
        let planned = plan_state_operation(
            &network,
            &operation,
            CarrierPlan {
                recipient,
                value: NoteValue::from_raw(1),
            },
            SuccessorTransport {
                ovk: None,
                memo: [0; 512],
            },
            OperationFunding {
                funding_spends: vec![FundingSpend {
                    fvk: fvk.clone(),
                    note: *funding_note.note(),
                }],
                change_outputs: vec![ChangeOutput {
                    fvk: fvk.clone(),
                    ovk: None,
                    recipient: fvk.address_at(0u32, Scope::Internal),
                    value: NoteValue::from_raw(change_value),
                    memo: [0; 512],
                }],
            },
        )
        .map_err(|error| format!("plan Names REVEAL: {error:#}"))?;
        if planned.planned_shape != shape {
            return Err("Names REVEAL shape changed after fee planning".to_string());
        }
        let built = build_names_v1_bundle(planned.plan, rand_10::rng())
            .map_err(|error| format!("build Names REVEAL bundle: {error:#}"))?;
        let built = build_names_v1_pczt(NamesV1PcztPlan {
            ironwood: built,
            params: network,
            consensus_branch_id: BranchId::Nu6_3,
            expiry_height: BlockHeight::from_u32(expiry_height),
            fallback_lock_time: 0,
        })
        .map_err(|error| format!("build Names REVEAL PCZT: {error:#}"))?;
        let finalized = finalize_names_v1_pczt_io(built)
            .map_err(|error| format!("finalize Names REVEAL PCZT: {error:#}"))?;
        let anchor_height = db
            .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
            .map_err(|error| format!("read Names anchor: {error}"))?
            .ok_or_else(|| "wallet has no Names anchor height".to_string())?
            .1;
        let (anchor, paths) = with_wallet_db_write_lock("names.read_reveal_witnesses", || {
            db.with_ironwood_tree_mut::<_, _, SqliteClientError>(|tree| {
                let anchor = tree.root_at_checkpoint_id(&anchor_height)?;
                let paths = [
                    registration_note.note_commitment_tree_position(),
                    funding_note.note_commitment_tree_position(),
                ]
                .map(|position| tree.witness_at_checkpoint_id_caching(position, &anchor_height));
                Ok((anchor, paths))
            })
        })
        .map_err(|error| format!("read Names witnesses: {error}"))?
        .ok_or_else(|| "wallet has no Ironwood commitment tree".to_string())?;
        let anchor: orchard::Anchor = anchor
            .ok_or_else(|| "wallet has no Ironwood anchor root".to_string())?
            .into();
        let paths = paths.map(|path| {
            path.map_err(|error| format!("read Names witness: {error:?}"))?
                .ok_or_else(|| "wallet has no Ironwood witness at anchor".to_string())
                .map(Into::into)
        });
        let [path0, path1] = paths;
        let witnessed = install_names_v1_ironwood_witnesses(
            finalized,
            NamesV1WitnessPlan {
                anchor,
                spends: vec![
                    NamesV1IronwoodWitness {
                        nullifier: registration_nf,
                        merkle_path: path0?,
                    },
                    NamesV1IronwoodWitness {
                        nullifier: funding_nf,
                        merkle_path: path1?,
                    },
                ],
            },
        )
        .map_err(|error| format!("install Names witnesses: {error:#}"))?;
        let consensus_key = orchard::circuit::ProvingKey::build(
            orchard::bundle::BundleVersion::ironwood_v3().circuit_version(),
        );
        let proved = prove_names_v1_ironwood_pczt(witnessed, &consensus_key)
            .map_err(|error| format!("prove Names Ironwood transaction: {error:#}"))?;
        let signed = sign_names_v1_ironwood_pczt(
            proved,
            NamesV1SigningPlan {
                spends: vec![
                    NamesV1IronwoodSigningKey {
                        nullifier: registration_nf,
                        ask: ask.clone(),
                    },
                    NamesV1IronwoodSigningKey {
                        nullifier: funding_nf,
                        ask,
                    },
                ],
            },
        )
        .map_err(|error| format!("sign Names REVEAL: {error:#}"))?;
        let extracted = extract_names_v1_transaction(signed)
            .map_err(|error| format!("extract Names REVEAL: {error:#}"))?;
        let txid = extracted.txid.into();
        let mut raw = Vec::new();
        extracted
            .transaction
            .write(&mut raw)
            .map_err(|error| format!("encode Names REVEAL: {error}"))?;
        Ok(NamesRevealTransaction {
            raw,
            txid,
            name: registration.name,
            account_uuid: account_uuid.to_string(),
            db_path: db_path.to_string(),
            network,
            valid_from_height: construction_height,
            expiry_height,
            fee_zatoshi: fee.into_u64(),
            fee_reservation: Some(fee_reservation),
        })
    })();
    authorized
}

fn store_reviewed_reveal_capability(
    transaction: NamesRevealTransaction,
    send_flow_id: &str,
) -> Result<u64, String> {
    let reservation = Arc::new(Mutex::new(transaction.fee_reservation));
    let release_reservation = reservation.clone();
    let release = Arc::new(move || {
        let mut reservation = release_reservation
            .lock()
            .map_err(|error| format!("lock Names REVEAL fee reservation: {error}"))?;
        // Dropping the armed reservation unlocks the temporary fee note.
        let _ = reservation.take();
        Ok(())
    });
    let retain_reservation = reservation.clone();
    let retain = Arc::new(move |_lock: sync::NamesRevealLockMetadata| {
        let mut reservation = retain_reservation
            .lock()
            .map_err(|error| format!("lock Names REVEAL fee reservation: {error}"))?;
        // The DB lock already has the bounded expiry set by build_reveal.
        // Disarm only the local Drop unlock path after broadcast acceptance.
        if let Some(reservation) = reservation.as_mut() {
            reservation.disarm();
        }
        Ok(())
    });
    let execution = sync::NamesRevealExecution {
        raw: transaction.raw,
        txid: transaction.txid,
        db_path: transaction.db_path,
        network: transaction.network,
        account_uuid: transaction.account_uuid,
        name: transaction.name,
        valid_from_height: transaction.valid_from_height,
        expiry_height: transaction.expiry_height,
        fee_zatoshi: transaction.fee_zatoshi,
    };
    sync::allocate_names_reveal_capability(
        send_flow_id,
        execution,
        sync::NamesRevealLockMetadata {
            expiry_height: u64::from(transaction.expiry_height),
        },
        sync::NamesRevealCleanup::Callbacks { release, retain },
    )
}

/// Builds, signs, and stores a reviewed REVEAL capability without broadcasting.
pub(crate) async fn begin_reviewed_reveal(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    send_flow_id: &str,
    seed: SecretVec<u8>,
) -> Result<NamesRevealProposal, String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    ensure_live_construction_window(lightwalletd_url, context.tip_height).await?;
    let transaction = build_reveal(db_path, network, account_uuid, name, seed)?;
    ensure_broadcast_window_open(
        lightwalletd_url,
        transaction.valid_from_height,
        transaction.expiry_height,
    )
    .await?;
    let fee_zatoshi = transaction.fee_zatoshi;
    let proposal_id = store_reviewed_reveal_capability(transaction, send_flow_id)?;
    Ok(NamesRevealProposal {
        proposal_id,
        fee_zatoshi,
    })
}

/// Reads the live chain tip from the wallet's lightwalletd endpoint.
async fn live_chain_tip(lightwalletd_url: &str) -> Result<u32, String> {
    let mut client = crate::wallet::sync_engine::open_isolated_lwd_channel(lightwalletd_url)
        .await
        .map_err(|error| format!("open chain-tip channel: {error}"))?;
    let tip = crate::wallet::sync_engine::get_latest_block(&mut client)
        .await
        .map_err(|error| format!("read chain tip: {error}"))?;
    u32::try_from(tip.height).map_err(|_| "chain tip exceeds supported height".to_string())
}

/// The Names sidecar tip advances only during wallet sync, so it can lag the
/// live chain tip. Construction needs current canonical Names state and note
/// witnesses even though the completed REVEAL or RENEW may remain valid over
/// later blocks. Fail fast until the wallet is current.
async fn ensure_live_construction_window(
    lightwalletd_url: &str,
    sidecar_tip: u32,
) -> Result<(), String> {
    let live_tip = live_chain_tip(lightwalletd_url).await?;
    if live_tip == sidecar_tip {
        return Ok(());
    }
    Err(format!(
        "the wallet is not current with the chain tip (wallet {sidecar_tip}, \
         chain {live_tip}); sync before building the Names operation"
    ))
}

/// Re-checks that the next possible inclusion height remains inside the
/// operation's canonical validity window.
fn broadcast_window_open_at_tip(live_tip: u32, valid_from_height: u32, expiry_height: u32) -> bool {
    live_tip
        .checked_add(1)
        .is_some_and(|next_height| next_height >= valid_from_height && next_height <= expiry_height)
}

pub(crate) async fn ensure_broadcast_window_open(
    lightwalletd_url: &str,
    valid_from_height: u32,
    expiry_height: u32,
) -> Result<(), String> {
    let live_tip = live_chain_tip(lightwalletd_url).await?;
    let next_height = live_tip
        .checked_add(1)
        .ok_or_else(|| "chain height overflow".to_string())?;
    if broadcast_window_open_at_tip(live_tip, valid_from_height, expiry_height) {
        return Ok(());
    }
    Err(format!(
        "the Names operation is outside its valid block window \
         ({valid_from_height}..={expiry_height}; next block is {next_height})"
    ))
}

pub(crate) async fn reveal_registration(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    seed: SecretVec<u8>,
) -> Result<[u8; 32], String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    ensure_live_construction_window(lightwalletd_url, context.tip_height).await?;
    let mut transaction = build_reveal(db_path, network, account_uuid, name, seed)?;
    ensure_broadcast_window_open(
        lightwalletd_url,
        transaction.valid_from_height,
        transaction.expiry_height,
    )
    .await?;
    sync::broadcast_raw_transaction_isolated(lightwalletd_url, &transaction.raw).await?;
    if let Some(reservation) = transaction.fee_reservation.as_mut() {
        reservation.disarm();
    }
    sync::decrypt_and_store_transaction(db_path, network, &transaction.raw, None)?;
    coppice::record_reveal_broadcast(db_path, account_uuid, &transaction.name, transaction.txid)?;
    Ok(transaction.txid)
}

/// Reconciles wallet output locks with replay-authenticated managed heads.
/// This is protection only: canonical applicability remains owned by the
/// Names runtime. A normal wallet send must never consume a current state note.
pub(crate) fn protect_managed_heads(
    db_path: &str,
    network: WalletNetwork,
    host: &coppice::NamesWalletHost,
) -> Result<(), String> {
    with_wallet_db_write_lock("names.protect_managed_heads", || {
        protect_managed_heads_locked(db_path, network, host)
    })
}

fn protect_managed_heads_locked(
    db_path: &str,
    network: WalletNetwork,
    host: &coppice::NamesWalletHost,
) -> Result<(), String> {
    let mut db = open_wallet_db(db_path, network)?;
    for (account_uuid, head) in host.managed_heads() {
        let account_id = parse_account_uuid(&account_uuid)?;
        let notes = db
            .get_unspent_ironwood_notes_at_historical_height(
                account_id,
                BlockHeight::from_u32(host.tip_height()),
            )
            .map_err(|error| format!("read managed Names notes: {error}"))?;
        for note in notes {
            if ExtractedNoteCommitment::from(note.note().commitment()).to_bytes() != head.commitment
            {
                continue;
            }
            let output = OutputRef::new(
                *note.txid(),
                PoolType::Shielded(ShieldedPool::Ironwood),
                u32::from(note.output_index()),
            );
            let custody_base = match head.data.status {
                coppice_names::v1::StateStatus::Active => head.data.lease_expiry,
                coppice_names::v1::StateStatus::Released => head.data.terminal_height,
            };
            let expiry = BlockHeight::from_u32(
                custody_base
                    .saturating_add(host.params().reuse_delay_blocks)
                    .saturating_add(2),
            );
            db.lock_outputs(&[output], LockOwner::new(head.commitment), expiry)
                .map_err(|error| format!("protect managed Names state note: {error:?}"))?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) enum NamesTransitionKind {
    Update(String),
    Renew,
    Release,
}

impl NamesTransitionKind {
    fn label(&self) -> &'static str {
        match self {
            Self::Update(_) => "update",
            Self::Renew => "renew",
            Self::Release => "release",
        }
    }
}

/// Builds UPDATE, RENEW, or RELEASE against the exact replay-authenticated
/// current head. The proof remains the local semantic authority; this host
/// supplies canonical history, wallet custody, witnesses and transport.
pub(crate) fn build_transition(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    kind: NamesTransitionKind,
    seed: SecretVec<u8>,
) -> Result<NamesRevealTransaction, String> {
    let canonical_name = name.trim().trim_end_matches(".zec").to_ascii_lowercase();
    let predecessor = coppice::accepted_head(db_path, network, &canonical_name)?
        .ok_or_else(|| "the name has no accepted canonical head".to_string())?;
    let context = coppice::lifecycle_context(db_path, network)?;
    let height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names construction height overflow".to_string())?;
    if !predecessor.is_active_at(height) {
        return Err("the accepted name is not active at the next block".to_string());
    }
    let expiry_height = match &kind {
        NamesTransitionKind::Renew => predecessor
            .data
            .lease_expiry
            .checked_sub(1)
            .ok_or_else(|| "Names RENEW validity window underflow".to_string())?,
        NamesTransitionKind::Update(_) | NamesTransitionKind::Release => height,
    };
    let account_id = parse_account_uuid(account_uuid)?;
    let mut db = open_wallet_db(db_path, network)?;
    let account = db
        .get_account(account_id)
        .map_err(|error| format!("read Names account: {error}"))?
        .ok_or_else(|| "Names account not found".to_string())?;
    let zip32_index = account
        .source()
        .key_derivation()
        .ok_or_else(|| "Names management requires a software-derived account".to_string())?
        .account_index();
    let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), zip32_index)
        .map_err(|error| format!("derive Names spending key: {error:?}"))?;
    let fvk = FullViewingKey::from(usk.orchard());
    let ask = SpendAuthorizingKey::from(usk.orchard());
    if predecessor.data.owner_pk != spend_auth_owner_key_bytes(&ask) {
        return Err("selected wallet account does not own this name".to_string());
    }
    let notes = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(context.tip_height),
        )
        .map_err(|error| format!("read managed Names notes: {error}"))?;
    let mut state_note = None;
    let mut funding_note = None;
    let mut funding_value = 0u64;
    for candidate in notes {
        if ExtractedNoteCommitment::from(candidate.note().commitment()).to_bytes()
            == predecessor.commitment
        {
            state_note = Some(candidate);
            continue;
        }
        // One funding note must cover the carrier outputs plus the fee on its
        // own, so the largest spendable candidate is the only choice that can
        // possibly be adequate.
        let spendable = db
            .get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                TargetHeight::from(BlockHeight::from_u32(height)),
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .map_err(|error| format!("classify Names fee note: {error}"))?;
        if spendable.is_some() {
            let value = candidate.note().value().inner();
            if value > funding_value {
                funding_value = value;
                funding_note = Some(candidate);
            }
        }
    }
    let state_note = state_note
        .ok_or_else(|| "wallet does not hold the canonical Names state note".to_string())?;
    let funding_note = funding_note.ok_or_else(|| {
        "wallet needs a separate confirmed Ironwood note for Names fees".to_string()
    })?;
    let state_nf = state_note.note().nullifier(&fvk).to_bytes();
    let funding_nf = funding_note.note().nullifier(&fvk).to_bytes();
    let inputs = TransitionInputs {
        predecessor,
        predecessor_note: *state_note.note(),
        scope: state_note.spending_key_scope(),
        fvk: fvk.clone(),
        ask: ask.clone(),
        operation_height: height,
        designated_action_index: 4,
        successor_seed: operation_seed(seed.expose_secret(), state_nf, kind.label().as_bytes()),
    };
    let preparation = match &kind {
        NamesTransitionKind::Update(address) => {
            let record = PaymentRecord::new(context.payment_network, address)
                .map_err(|error| format!("invalid UPDATE payment address: {error:?}"))?
                .encode();
            prepare_update(inputs, record, context.params)
        }
        NamesTransitionKind::Renew => prepare_renew(inputs, context.params),
        NamesTransitionKind::Release => prepare_release(inputs, context.params),
    }
    .map_err(|error| format!("prepare Names {}: {error:#}", kind.label()))?;
    let funding_ref = OutputRef::new(
        *funding_note.txid(),
        PoolType::Shielded(ShieldedPool::Ironwood),
        u32::from(funding_note.output_index()),
    );
    let funding_lock_owner = LockOwner::new(preparation.statement().successor_commitment);
    with_wallet_db_write_lock("names.reserve_transition_fee_note", || {
        db.lock_outputs(
            &[funding_ref],
            funding_lock_owner,
            BlockHeight::from_u32(expiry_height),
        )
        .map_err(|error| format!("reserve Names {} fee note: {error:?}", kind.label()))
    })?;
    let fee_reservation = NamesFeeReservation {
        db_path: db_path.to_string(),
        network,
        output: Some(funding_ref),
        owner: funding_lock_owner,
    };
    // Same reservation-release guarantee as REVEAL: a failure after the
    // fee-note lock must never strand the note until lock expiry. Expensive
    // proving stays outside the wallet write lock.
    let authorized = (|| -> Result<NamesRevealTransaction, String> {
        drop(seed);
        let proof = OrchardV1ProofProver::new()
            .prove_transition(
                preparation.statement(),
                preparation.witness().clone(),
                rand_10::rng(),
            )
            .map_err(|error| format!("prove Names {}: {error:?}", kind.label()))?;
        let operation = preparation
            .finalize(proof, context.core_runtime_id)
            .map_err(|error| format!("finalize Names {}: {error:#}", kind.label()))?;
        let (shape, fee) = planned_state_operation_shape_and_fee(&network, &operation, 1, 1)
            .map_err(|error| format!("plan Names {} fee: {error:#}", kind.label()))?;
        let carrier_total = u64::try_from(operation.frames().len())
            .map_err(|_| "Names carrier count exceeds u64".to_string())?;
        let change_value = funding_note
            .note()
            .value()
            .inner()
            .checked_sub(carrier_total)
            .and_then(|value| value.checked_sub(fee.into_u64()))
            .ok_or_else(|| "separate Ironwood note cannot cover Names fee".to_string())?;
        let recipient = Option::<orchard::Address>::from(orchard::Address::from_raw_address_bytes(
            &context.rendezvous_receiver,
        ))
        .ok_or_else(|| "configured Names rendezvous receiver is invalid".to_string())?;
        let planned = plan_state_operation(
            &network,
            &operation,
            CarrierPlan {
                recipient,
                value: NoteValue::from_raw(1),
            },
            SuccessorTransport {
                ovk: None,
                memo: [0; 512],
            },
            OperationFunding {
                funding_spends: vec![FundingSpend {
                    fvk: fvk.clone(),
                    note: *funding_note.note(),
                }],
                change_outputs: vec![ChangeOutput {
                    fvk: fvk.clone(),
                    ovk: None,
                    recipient: fvk.address_at(0u32, Scope::Internal),
                    value: NoteValue::from_raw(change_value),
                    memo: [0; 512],
                }],
            },
        )
        .map_err(|error| format!("plan Names {}: {error:#}", kind.label()))?;
        if planned.planned_shape != shape {
            return Err(format!(
                "Names {} shape changed after fee planning",
                kind.label()
            ));
        }
        let built = build_names_v1_bundle(planned.plan, rand_10::rng())
            .map_err(|error| format!("build Names {} bundle: {error:#}", kind.label()))?;
        let built = build_names_v1_pczt(NamesV1PcztPlan {
            ironwood: built,
            params: network,
            consensus_branch_id: BranchId::Nu6_3,
            expiry_height: BlockHeight::from_u32(expiry_height),
            fallback_lock_time: 0,
        })
        .map_err(|error| format!("build Names {} PCZT: {error:#}", kind.label()))?;
        let finalized = finalize_names_v1_pczt_io(built)
            .map_err(|error| format!("finalize Names {} PCZT: {error:#}", kind.label()))?;
        let anchor_height = db
            .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
            .map_err(|error| format!("read Names anchor: {error}"))?
            .ok_or_else(|| "wallet has no Names anchor height".to_string())?
            .1;
        let (anchor, paths) = with_wallet_db_write_lock("names.read_transition_witnesses", || {
            db.with_ironwood_tree_mut::<_, _, SqliteClientError>(|tree| {
                let anchor = tree.root_at_checkpoint_id(&anchor_height)?;
                let paths = [
                    state_note.note_commitment_tree_position(),
                    funding_note.note_commitment_tree_position(),
                ]
                .map(|position| tree.witness_at_checkpoint_id_caching(position, &anchor_height));
                Ok((anchor, paths))
            })
        })
        .map_err(|error| format!("read Names witnesses: {error}"))?
        .ok_or_else(|| "wallet has no Ironwood commitment tree".to_string())?;
        let anchor: orchard::Anchor = anchor
            .ok_or_else(|| "wallet has no Ironwood anchor root".to_string())?
            .into();
        let paths = paths.map(|path| {
            path.map_err(|error| format!("read Names witness: {error:?}"))?
                .ok_or_else(|| "wallet has no Ironwood witness at anchor".to_string())
                .map(Into::into)
        });
        let [path0, path1] = paths;
        let witnessed = install_names_v1_ironwood_witnesses(
            finalized,
            NamesV1WitnessPlan {
                anchor,
                spends: vec![
                    NamesV1IronwoodWitness {
                        nullifier: state_nf,
                        merkle_path: path0?,
                    },
                    NamesV1IronwoodWitness {
                        nullifier: funding_nf,
                        merkle_path: path1?,
                    },
                ],
            },
        )
        .map_err(|error| format!("install Names witnesses: {error:#}"))?;
        let consensus_key = orchard::circuit::ProvingKey::build(
            orchard::bundle::BundleVersion::ironwood_v3().circuit_version(),
        );
        let proved = prove_names_v1_ironwood_pczt(witnessed, &consensus_key)
            .map_err(|error| format!("prove Names Ironwood transaction: {error:#}"))?;
        let signed = sign_names_v1_ironwood_pczt(
            proved,
            NamesV1SigningPlan {
                spends: vec![
                    NamesV1IronwoodSigningKey {
                        nullifier: state_nf,
                        ask: ask.clone(),
                    },
                    NamesV1IronwoodSigningKey {
                        nullifier: funding_nf,
                        ask,
                    },
                ],
            },
        )
        .map_err(|error| format!("sign Names {}: {error:#}", kind.label()))?;
        let extracted = extract_names_v1_transaction(signed)
            .map_err(|error| format!("extract Names {}: {error:#}", kind.label()))?;
        let txid = extracted.txid.into();
        let mut raw = Vec::new();
        extracted
            .transaction
            .write(&mut raw)
            .map_err(|error| format!("encode Names {}: {error}", kind.label()))?;
        Ok(NamesRevealTransaction {
            raw,
            txid,
            name: canonical_name,
            account_uuid: account_uuid.to_string(),
            db_path: db_path.to_string(),
            network,
            valid_from_height: height,
            expiry_height,
            fee_zatoshi: fee.into_u64(),
            fee_reservation: Some(fee_reservation),
        })
    })();
    authorized
}

pub(crate) async fn execute_transition(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    kind: NamesTransitionKind,
    seed: SecretVec<u8>,
) -> Result<[u8; 32], String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    ensure_live_construction_window(lightwalletd_url, context.tip_height).await?;
    let mut transaction = build_transition(db_path, network, account_uuid, name, kind, seed)?;
    ensure_broadcast_window_open(
        lightwalletd_url,
        transaction.valid_from_height,
        transaction.expiry_height,
    )
    .await?;
    sync::broadcast_raw_transaction_isolated(lightwalletd_url, &transaction.raw).await?;
    if let Some(reservation) = transaction.fee_reservation.as_mut() {
        reservation.disarm();
    }
    sync::decrypt_and_store_transaction(db_path, network, &transaction.raw, None)?;
    Ok(transaction.txid)
}

#[cfg(test)]
mod tests {
    use super::{
        broadcast_window_open_at_tip, canonical_registration_name, registration_payment_record,
        StoredRegistration,
    };
    use coppice_names::v1::{PaymentNetwork, PaymentRecord};

    const MAINNET_UA: &str = "u1pg2aaph7jp8rpf6yhsza25722sg5fcn3vaca6ze27hqjw7jvvhhuxkpcg0ge9xh6drsgdkda8qjq5chpehkcpxf87rnjryjqwymdheptpvnljqqrjqzjwkc2ma6hcq666kgwfytxwac8eyex6ndgr6ezte66706e3vaqrd25dzvzkc69kw0jgywtd0cmq52q5lkw6uh7hyvzjse8ksx";

    fn registration(record: Vec<u8>) -> StoredRegistration {
        StoredRegistration {
            account_uuid: "account".to_string(),
            name: "alice".to_string(),
            record,
            nonce: [1; 32],
            commitment: [2; 32],
            send_flow_id: None,
            bond_txid: None,
            bond_output_index: None,
            commit_height: None,
            phase: "awaiting_bond".to_string(),
            commit_txid: None,
            reveal_txid: None,
        }
    }

    #[test]
    fn reviewed_names_operation_survives_block_advances_inside_its_window() {
        assert!(broadcast_window_open_at_tip(100, 101, 115));
        assert!(broadcast_window_open_at_tip(108, 101, 115));
        assert!(broadcast_window_open_at_tip(114, 101, 115));
        assert!(!broadcast_window_open_at_tip(115, 101, 115));
    }

    #[test]
    fn registration_name_validation_matches_protocol_rules() {
        assert_eq!(
            canonical_registration_name(" Alice-42 ").unwrap(),
            "alice-42"
        );
        for invalid in [
            "",
            ".zec",
            "alice.zec",
            "-alice",
            "alice-",
            "alice_name",
            "ليس",
        ] {
            assert!(canonical_registration_name(invalid).is_err(), "{invalid}");
        }
        assert!(canonical_registration_name(&"a".repeat(63)).is_ok());
        assert!(canonical_registration_name(&"a".repeat(64)).is_err());
    }

    #[test]
    fn existing_registration_payment_record_is_validated_fail_closed() {
        let encoded = PaymentRecord::new(PaymentNetwork::Main, MAINNET_UA)
            .unwrap()
            .encode();
        let valid = registration(encoded.clone());
        assert_eq!(
            registration_payment_record(Some(&valid), PaymentNetwork::Main, MAINNET_UA).unwrap(),
            encoded
        );

        let malformed = registration(vec![1, 2, 3]);
        assert!(
            registration_payment_record(Some(&malformed), PaymentNetwork::Main, MAINNET_UA)
                .unwrap_err()
                .contains("stored Names payment record is invalid")
        );
        assert!(
            registration_payment_record(Some(&valid), PaymentNetwork::Test, MAINNET_UA).is_err()
        );
    }
}
