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
    let canonical_name = name.trim().to_ascii_lowercase();
    if canonical_name.is_empty() || canonical_name.contains('.') {
        return Err("enter the name label only; .zec is added by the wallet".to_string());
    }
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
    let commitment = prepare_commit(&intent)
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
pub(crate) fn reserve_pending_bonds(db_path: &str, network: WalletNetwork) -> Result<(), String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    let pending = coppice::registrations(db_path)?;
    let mut db = open_wallet_db(db_path, network)?;
    let target_height = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .map_err(|error| format!("read Names bond target height: {error}"))?
        .ok_or_else(|| "wallet has no synchronized target height".to_string())?
        .0;
    for mut registration in pending
        .into_iter()
        .filter(|registration| registration.phase == "awaiting_bond")
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
                .saturating_add(context.params.commit_ttl_blocks)
                .saturating_add(2),
        );
        db.lock_outputs(&[output], LockOwner::new(registration.commitment), expiry)
            .map_err(|error| format!("reserve prepared Names bond: {error:?}"))?;
        registration.bond_txid = Some((*note.txid()).into());
        registration.bond_output_index = Some(u32::from(note.output_index()));
        registration.phase = "bond_reserved".to_string();
        coppice::replace_registration(db_path, registration)?;
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
    let canonical_name = name.trim().to_ascii_lowercase();
    if canonical_name.is_empty() || canonical_name.contains('.') {
        return Err("enter the name label only; .zec is added by the wallet".to_string());
    }

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
    let record = match &existing {
        Some(registration) => registration.record.clone(),
        None => PaymentRecord::new(context.payment_network, payment_address)
            .map_err(|error| format!("invalid Names payment record: {error:?}"))?
            .encode(),
    };
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
        record: existing
            .as_ref()
            .map(|registration| registration.record.clone())
            .unwrap_or_else(|| record.clone()),
        secret: registration_secret(seed.expose_secret(), account_uuid, &canonical_name, nonce),
    };
    drop(seed);
    let prepared =
        prepare_commit(&intent).map_err(|error| format!("prepare Names COMMIT: {error:#}"))?;

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
    db.lock_outputs(&[bond_ref], lock_owner, lock_expiry)
        .map_err(|error| format!("reserve one-ZEC Names bond: {error:?}"))?;
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
            let mut db = open_wallet_db(db_path, network)?;
            let _ = db.unlock_output(&bond_ref, lock_owner);
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
        let mut db = open_wallet_db(db_path, network)?;
        let _ = db.unlock_output(&bond_ref, lock_owner);
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
    let mut db = open_wallet_db(db_path, network)?;
    db.unlock_output(&output, LockOwner::new(registration.commitment))
        .map(|_| ())
        .map_err(|error| format!("release cancelled Names bond: {error:?}"))
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
            let mut db = open_wallet_db(db_path, network)?;
            let _ = db.unlock_output(&output, LockOwner::new(registration.commitment));
        }
    }
    Ok(())
}

pub(crate) struct NamesRevealTransaction {
    pub raw: Vec<u8>,
    pub txid: [u8; 32],
    pub name: String,
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
    let commit =
        coppice::accepted_commit(db_path, network, registration.commitment)?.ok_or_else(|| {
            "the exact COMMIT is not yet accepted in canonical Names history".to_string()
        })?;
    let context = coppice::lifecycle_context(db_path, network)?;
    let construction_height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names construction height overflow".to_string())?;

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
    let name_id = intent
        .name_id()
        .map_err(|error| format!("derive Names name id: {error:?}"))?;
    let target_height = coppice_names::v1::schedule::next_anchor_height(
        name_id,
        construction_height,
        context.params,
    )
    .ok_or_else(|| "no future legal Names REVEAL height exists".to_string())?;
    if construction_height != target_height {
        return Err(format!(
            "REVEAL is scheduled for block {target_height}; wallet tip is {}",
            context.tip_height
        ));
    }

    let historical_height = context.tip_height;
    let candidates = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(historical_height),
        )
        .map_err(|error| format!("read Names operation notes: {error}"))?;
    let mut registration_note = None;
    let mut funding_note = None;
    for candidate in candidates {
        let candidate_txid: [u8; 32] = (*candidate.txid()).into();
        if registration.bond_txid == Some(candidate_txid)
            && registration.bond_output_index == Some(u32::from(candidate.output_index()))
        {
            registration_note = Some(candidate);
            continue;
        }
        if funding_note.is_none() {
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
    db.lock_outputs(
        &[funding_ref],
        LockOwner::new(preparation.statement().commitment),
        BlockHeight::from_u32(construction_height.saturating_add(2)),
    )
    .map_err(|error| format!("reserve Names REVEAL fee note: {error:?}"))?;
    drop(seed);
    let proof = OrchardV1ProofProver::new()
        .prove_genesis(
            preparation.statement(),
            preparation.witness().clone(),
            rand_10::rng(),
        )
        .map_err(|error| format!("prove Names REVEAL: {error:?}"))?;
    let operation = preparation
        .finalize(proof)
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
        expiry_height: BlockHeight::from_u32(construction_height),
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
    let (anchor, paths) = db
        .with_ironwood_tree_mut::<_, _, SqliteClientError>(|tree| {
            let anchor = tree.root_at_checkpoint_id(&anchor_height)?;
            let paths = [
                registration_note.note_commitment_tree_position(),
                funding_note.note_commitment_tree_position(),
            ]
            .map(|position| tree.witness_at_checkpoint_id_caching(position, &anchor_height));
            Ok((anchor, paths))
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
    })
}

pub(crate) async fn reveal_registration(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    seed: SecretVec<u8>,
) -> Result<[u8; 32], String> {
    let transaction = build_reveal(db_path, network, account_uuid, name, seed)?;
    sync::broadcast_raw_transaction_isolated(lightwalletd_url, &transaction.raw).await?;
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
    for candidate in notes {
        if ExtractedNoteCommitment::from(candidate.note().commitment()).to_bytes()
            == predecessor.commitment
        {
            state_note = Some(candidate);
            continue;
        }
        if funding_note.is_none()
            && db
                .get_spendable_note(
                    candidate.txid(),
                    ShieldedPool::Ironwood,
                    u32::from(candidate.output_index()),
                    TargetHeight::from(BlockHeight::from_u32(height)),
                    LockFilter::Policy(&LockedInputPolicy::Exclude),
                )
                .map_err(|error| format!("classify Names fee note: {error}"))?
                .is_some()
        {
            funding_note = Some(candidate);
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
    db.lock_outputs(
        &[funding_ref],
        LockOwner::new(preparation.statement().successor_commitment),
        BlockHeight::from_u32(height.saturating_add(2)),
    )
    .map_err(|error| format!("reserve Names {} fee note: {error:?}", kind.label()))?;
    drop(seed);
    let proof = OrchardV1ProofProver::new()
        .prove_transition(
            preparation.statement(),
            preparation.witness().clone(),
            rand_10::rng(),
        )
        .map_err(|error| format!("prove Names {}: {error:?}", kind.label()))?;
    let operation = preparation
        .finalize(proof)
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
        expiry_height: BlockHeight::from_u32(height),
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
    let (anchor, paths) = db
        .with_ironwood_tree_mut::<_, _, SqliteClientError>(|tree| {
            let anchor = tree.root_at_checkpoint_id(&anchor_height)?;
            let paths = [
                state_note.note_commitment_tree_position(),
                funding_note.note_commitment_tree_position(),
            ]
            .map(|position| tree.witness_at_checkpoint_id_caching(position, &anchor_height));
            Ok((anchor, paths))
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
    })
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
    let transaction = build_transition(db_path, network, account_uuid, name, kind, seed)?;
    sync::broadcast_raw_transaction_isolated(lightwalletd_url, &transaction.raw).await?;
    sync::decrypt_and_store_transaction(db_path, network, &transaction.raw, None)?;
    Ok(transaction.txid)
}
