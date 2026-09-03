//! Wallet custody and transaction lifecycle for the replacement Names protocol.

use coppice_names::{
    proof::keygen,
    protocol::{CanonicalUa, FieldElement, Name},
};
use coppice_names_wallet::{
    builder::{
        build_names_bundle, build_names_pczt, build_ordinary_ironwood_spend,
        extract_names_transaction, finalize_names_pczt_io, install_names_ironwood_witnesses,
        names_ironwood_shape_from_counts, prove_names_ironwood_pczt, required_zip317_fee_for_names,
        sign_names_ironwood_pczt, ChangeOutput, FundingSpend, NamesIronwoodSigningKey,
        NamesIronwoodWitness, NamesPcztPlan, NamesSigningPlan, NamesWitnessPlan,
        OrdinaryIronwoodSpendPlan,
    },
    classify_bond_inventory,
    recovery::{derive_name_spending_key, derive_refresh_bond_note, derive_reveal_bond_note},
    replacement::{prepare_commit, prepare_refresh, prepare_reveal, RefreshInputs, RevealInputs},
    BondInventoryDecision, REQUIRED_BOND_ZATOSHIS,
};
use orchard::{
    keys::{FullViewingKey, Scope, SpendAuthorizingKey},
    value::NoteValue,
};
use secrecy::{ExposeSecret, SecretVec};
use std::sync::{Arc, Mutex};
use zcash_client_backend::data_api::{
    locking::{LockFilter, OutputLockStore},
    wallet::{input_selection::LockedInputPolicy, TargetHeight},
    Account as _, InputSource, WalletCommitmentTrees, WalletRead,
};
use zcash_client_backend::wallet::{LockOwner, OutputRef};
use zcash_client_sqlite::error::SqliteClientError;
use zcash_keys::{address::UnifiedAddress, keys::UnifiedSpendingKey};
use zcash_primitives::transaction::TxId;
use zcash_protocol::{
    consensus::{BlockHeight, BranchId, Parameters},
    PoolType, ShieldedPool,
};

use super::{
    coppice::{self, BondOrigin, StoredRegistration},
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
    let candidates = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(u32::from(target_height).saturating_sub(1)),
        )
        .map_err(|error| format!("read Ironwood bond inventory: {error}"))?;
    let mut values = Vec::new();
    for candidate in candidates {
        if let Some(note) = db
            .get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                target_height,
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .map_err(|error| format!("classify Ironwood bond candidate: {error}"))?
        {
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
    let spendable_ironwood_zatoshi = values.iter().copied().fold(0, u64::saturating_add);
    let state = match classify_bond_inventory(values) {
        BondInventoryDecision::Ready => "ready",
        BondInventoryDecision::PrepareExactNote => "needs_preparation",
        BondInventoryDecision::InsufficientFunds => "insufficient_funds",
    };
    Ok(NamesBondStatus {
        state: state.into(),
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

fn canonical_registration_name(value: &str) -> Result<Name, String> {
    let canonical = value.trim().to_ascii_lowercase();
    Name::parse(&canonical).map_err(|_| {
        "Names labels use 1-63 lowercase letters, digits, or hyphens, with no leading or trailing hyphen"
            .to_string()
    })
}

fn target_reveal(
    parameters: coppice_names::schedule::Parameters,
    name: &Name,
    next_height: u32,
) -> Result<(u32, u32), String> {
    let name_id = name
        .id()
        .map_err(|error| format!("derive Names identifier: {error:?}"))?;
    let earliest = next_height
        .checked_add(parameters.commit_maturity_blocks)
        .ok_or_else(|| "Names reveal height overflow".to_string())?;
    let mut epoch = parameters
        .epoch(next_height.max(parameters.activation_height))
        .map_err(|error| format!("derive Names epoch: {error:?}"))?;
    loop {
        let window = parameters
            .window(name_id, epoch)
            .map_err(|error| format!("derive Names operation window: {error:?}"))?;
        let reveal = window.start.max(earliest);
        if reveal < window.end {
            return Ok((epoch, reveal));
        }
        epoch = epoch
            .checked_add(1)
            .ok_or_else(|| "Names target epoch overflow".to_string())?;
    }
}

pub(crate) fn prepare_registration_draft(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    payment_address: &str,
    seed: SecretVec<u8>,
) -> Result<(), String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    let name = canonical_registration_name(name)?;
    if coppice::registration(db_path, account_uuid, name.as_str())?.is_some() {
        return Err("this account already manages that name".into());
    }
    let ua = CanonicalUa::parse(context.network, payment_address)
        .map_err(|error| format!("invalid canonical Unified Address: {error:?}"))?;
    let next_height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names construction height overflow".to_string())?;
    let (target_epoch, reveal_height) = target_reveal(context.parameters, &name, next_height)?;
    let prepared = prepare_commit(
        seed.expose_secret(),
        context.deployment,
        &name,
        reveal_height,
    )
    .map_err(|error| format!("prepare Names COMMIT: {error:#}"))?;
    if prepared.target_epoch() != target_epoch {
        return Err("Names COMMIT target epoch mismatch".into());
    }
    let commitment = match prepared.publication().operation() {
        coppice_names::codec::Operation::Commit { commitment } => commitment.to_bytes(),
        _ => return Err("prepared Names operation is not COMMIT".into()),
    };
    coppice::store_registration(
        db_path,
        StoredRegistration {
            account_uuid: account_uuid.into(),
            name: name.as_str().into(),
            ua: ua.as_str().into(),
            commitment,
            target_epoch,
            target_reveal_height: reveal_height,
            send_flow_id: None,
            bond_txid: None,
            bond_output_index: None,
            commit_height: None,
            commit_tx_index: None,
            phase: "awaiting_bond".into(),
            commit_txid: None,
            reveal_txid: None,
        },
    )?;
    reserve_pending_bonds(db_path, network)
}

pub(crate) fn reserve_pending_bonds(db_path: &str, network: WalletNetwork) -> Result<(), String> {
    with_wallet_db_write_lock("names.reserve_pending_bonds", || {
        reserve_pending_bonds_locked(db_path, network)
    })
}

fn reserve_pending_bonds_locked(db_path: &str, network: WalletNetwork) -> Result<(), String> {
    let metadata = coppice::configured_names_metadata(db_path, network)?;
    let registrations = coppice::registrations(db_path)?;
    let mut db = open_wallet_db(db_path, network)?;
    let target_height = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .map_err(|error| format!("read Names target height: {error}"))?
        .ok_or_else(|| "wallet has no synchronized target height".to_string())?
        .0;
    for mut registration in registrations {
        if registration.phase != "awaiting_bond" && registration.phase != "bond_reserved" {
            continue;
        }
        let account_id = parse_account_uuid(&registration.account_uuid)?;
        let candidates = db
            .get_unspent_ironwood_notes_at_historical_height(
                account_id,
                BlockHeight::from_u32(u32::from(target_height).saturating_sub(1)),
            )
            .map_err(|error| format!("read pending Names bonds: {error}"))?;
        let selected = candidates.into_iter().find(|candidate| {
            db.get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                target_height,
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .ok()
            .flatten()
            .is_some_and(|note| note.note().value().into_u64() == REQUIRED_BOND_ZATOSHIS)
        });
        let (txid, output_index) = match (
            registration.bond_txid,
            registration.bond_output_index,
            selected,
        ) {
            (Some(txid), Some(index), _) => (txid, index),
            (_, _, Some(note)) => ((*note.txid()).into(), u32::from(note.output_index())),
            _ => continue,
        };
        let output = OutputRef::new(
            TxId::from_bytes(txid),
            PoolType::Shielded(ShieldedPool::Ironwood),
            output_index,
        );
        let name = Name::parse(&registration.name)
            .map_err(|error| format!("stored Names label is invalid: {error:?}"))?;
        let window = metadata
            .parameters
            .window(
                name.id()
                    .map_err(|error| format!("derive stored name ID: {error:?}"))?,
                registration.target_epoch,
            )
            .map_err(|error| format!("derive stored Names window: {error:?}"))?;
        let expiry = BlockHeight::from_u32(
            window
                .end
                .max(u32::from(target_height).saturating_add(metadata.parameters.commit_ttl_blocks))
                .saturating_add(2),
        );
        db.lock_outputs(&[output], LockOwner::new(registration.commitment), expiry)
            .map_err(|error| format!("reserve Names bond: {error:?}"))?;
        registration.bond_txid = Some(txid);
        registration.bond_output_index = Some(output_index);
        registration.phase = "bond_reserved".into();
        coppice::replace_registration(db_path, registration)?;
    }
    Ok(())
}

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
    let name = canonical_registration_name(name)?;
    let mut registration = coppice::registration(db_path, account_uuid, name.as_str())?
        .ok_or_else(|| "prepare the Names registration draft first".to_string())?;
    if registration.phase == "awaiting_bond" {
        reserve_pending_bonds(db_path, network)?;
        registration = coppice::registration(db_path, account_uuid, name.as_str())?
            .ok_or_else(|| "Names registration draft disappeared".to_string())?;
    }
    if registration.phase != "bond_reserved" {
        return Err("Names registration does not have a reserved one-ZEC bond".into());
    }
    let ua = CanonicalUa::parse(context.network, payment_address)
        .map_err(|error| format!("invalid canonical Unified Address: {error:?}"))?;
    if ua.as_str() != registration.ua {
        return Err("this draft is bound to a different Unified Address".into());
    }
    let window = context
        .parameters
        .window(
            name.id()
                .map_err(|error| format!("derive name ID: {error:?}"))?,
            registration.target_epoch,
        )
        .map_err(|error| format!("derive target operation window: {error:?}"))?;
    let commit_height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names construction height overflow".to_string())?;
    if commit_height >= window.end {
        registration.phase = "window_missed".into();
        coppice::replace_registration(db_path, registration)?;
        return Err(
            "the draft's name window was missed; start this unbroadcast registration again".into(),
        );
    }
    let reveal_height = registration.target_reveal_height;
    if !window.contains(reveal_height) {
        return Err("stored REVEAL target is outside its name window".into());
    }
    if !context
        .parameters
        .accepts_commit(commit_height, reveal_height)
    {
        let commit_window_start =
            reveal_height.saturating_sub(context.parameters.commit_ttl_blocks.saturating_sub(1));
        if commit_height < commit_window_start {
            return Err(format!(
                "COMMIT window opens at height {commit_window_start}"
            ));
        }
        registration.phase = "commit_window_missed".into();
        coppice::replace_registration(db_path, registration)?;
        return Err("the draft's COMMIT window was missed; start again".into());
    }
    let prepared = prepare_commit(
        seed.expose_secret(),
        context.deployment,
        &name,
        reveal_height,
    )
    .map_err(|error| format!("rederive Names COMMIT: {error:#}"))?;
    let commitment = match prepared.publication().operation() {
        coppice_names::codec::Operation::Commit { commitment } => commitment.to_bytes(),
        _ => return Err("prepared Names operation is not COMMIT".into()),
    };
    if commitment != registration.commitment {
        return Err("mnemonic does not reproduce this Names registration".into());
    }
    let receiver = Option::<orchard::Address>::from(orchard::Address::from_raw_address_bytes(
        &context.rendezvous_receiver,
    ))
    .ok_or_else(|| "configured Names rendezvous receiver is invalid".to_string())?;
    let rendezvous = UnifiedAddress::from_receivers(Some(receiver), None, None)
        .ok_or_else(|| "construct Names rendezvous address".to_string())?
        .to_zcash_address(network.network_type())
        .to_string();
    let [frame] = prepared.publication().frames() else {
        return Err("Names COMMIT must fit exactly one carrier".into());
    };
    let proposal = sync::propose_send_with_raw_memo(
        db_path,
        network,
        account_uuid,
        send_flow_id,
        &rendezvous,
        0,
        Some(frame),
    )?;
    registration.send_flow_id = Some(send_flow_id.into());
    registration.phase = "commit_proposed".into();
    coppice::replace_registration(db_path, registration)?;
    Ok(NamesCommitProposal {
        proposal_id: proposal.proposal_id,
        fee_zatoshi: proposal.fee_zatoshi,
        commitment,
    })
}

pub(crate) fn cancel_registration_proposal(
    db_path: &str,
    network: WalletNetwork,
    send_flow_id: &str,
) -> Result<(), String> {
    let Some(registration) = coppice::take_cancelled_registration(db_path, send_flow_id)? else {
        return Ok(());
    };
    if let (Some(txid), Some(index)) = (registration.bond_txid, registration.bond_output_index) {
        let output = OutputRef::new(
            TxId::from_bytes(txid),
            PoolType::Shielded(ShieldedPool::Ironwood),
            index,
        );
        with_wallet_db_write_lock("names.release_cancelled_bond", || {
            let mut db = open_wallet_db(db_path, network)?;
            db.unlock_output(&output, LockOwner::new(registration.commitment))
                .map(|_| ())
                .map_err(|error| format!("release cancelled Names bond: {error:?}"))
        })?;
    }
    Ok(())
}

pub(crate) fn discard_registration_workflow(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
) -> Result<(), String> {
    let name = canonical_registration_name(name)?;
    let Some(registration) = coppice::registration(db_path, account_uuid, name.as_str())? else {
        return Ok(());
    };
    if !matches!(
        registration.phase.as_str(),
        "awaiting_bond" | "bond_reserved" | "window_missed" | "commit_expired"
    ) {
        return Err("only an unbroadcast registration workflow can be discarded".into());
    }
    let registration = coppice::take_registration_workflow(db_path, account_uuid, name.as_str())?
        .ok_or_else(|| "Names workflow disappeared".to_string())?;
    if let (Some(txid), Some(index)) = (registration.bond_txid, registration.bond_output_index) {
        let output = OutputRef::new(
            TxId::from_bytes(txid),
            PoolType::Shielded(ShieldedPool::Ironwood),
            index,
        );
        with_wallet_db_write_lock("names.release_discarded_bond", || {
            let mut db = open_wallet_db(db_path, network)?;
            let _ = db.unlock_output(&output, LockOwner::new(registration.commitment));
            Ok::<(), String>(())
        })?;
    }
    Ok(())
}

pub(crate) struct NamesTransaction {
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

pub(crate) fn build_reveal(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    seed: SecretVec<u8>,
) -> Result<NamesTransaction, String> {
    let name = canonical_registration_name(name)?;
    let registration = coppice::registration(db_path, account_uuid, name.as_str())?
        .ok_or_else(|| "this account has no pending registration for that name".to_string())?;
    if registration.phase == "reveal_broadcast" && registration.reveal_txid.is_some() {
        return Err("REVEAL is already broadcast and awaits confirmation".into());
    }
    let commit = coppice::accepted_commit(db_path, network, registration.commitment)?
        .ok_or_else(|| "the exact COMMIT is not accepted in canonical Names history".to_string())?;
    let context = coppice::lifecycle_context(db_path, network)?;
    let operation_height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names construction height overflow".to_string())?;
    let name_id = name
        .id()
        .map_err(|error| format!("derive name identifier: {error:?}"))?;
    let epoch = context
        .parameters
        .epoch(operation_height)
        .map_err(|error| format!("derive Names epoch: {error:?}"))?;
    if epoch != registration.target_epoch
        || !context
            .parameters
            .accepts_operation(name_id, operation_height)
        || !context
            .parameters
            .accepts_commit(commit.height, operation_height)
    {
        return Err("REVEAL must wait for its deterministic name window".into());
    }
    let window = context
        .parameters
        .window(name_id, epoch)
        .map_err(|error| format!("derive Names window: {error:?}"))?;
    let expiry_height = window.end.saturating_sub(1).min(
        commit
            .height
            .saturating_add(context.parameters.commit_ttl_blocks - 1),
    );

    let account_id = parse_account_uuid(account_uuid)?;
    let mut db = open_wallet_db(db_path, network)?;
    let account = db
        .get_account(account_id)
        .map_err(|error| format!("read Names account: {error}"))?
        .ok_or_else(|| "Names account not found".to_string())?;
    let zip32_index = account
        .source()
        .key_derivation()
        .ok_or_else(|| "Names requires a software-derived account".to_string())?
        .account_index();
    let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), zip32_index)
        .map_err(|error| format!("derive wallet spending key: {error:?}"))?;
    let wallet_fvk = FullViewingKey::from(usk.orchard());
    let wallet_ask = SpendAuthorizingKey::from(usk.orchard());
    let ua = CanonicalUa::parse(context.network, &registration.ua)
        .map_err(|error| format!("stored canonical UA is invalid: {error:?}"))?;

    let candidates = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(context.tip_height),
        )
        .map_err(|error| format!("read Names operation notes: {error}"))?;
    let mut bond = None;
    let mut funding = None;
    let mut funding_value = 0u64;
    for candidate in candidates {
        let txid: [u8; 32] = (*candidate.txid()).into();
        if registration.bond_txid == Some(txid)
            && registration.bond_output_index == Some(u32::from(candidate.output_index()))
        {
            bond = Some(candidate);
            continue;
        }
        let spendable = db
            .get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                TargetHeight::from(BlockHeight::from_u32(operation_height)),
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .map_err(|error| format!("classify Names fee note: {error}"))?;
        if spendable.is_some() && candidate.note().value().inner() > funding_value {
            funding_value = candidate.note().value().inner();
            funding = Some(candidate);
        }
    }
    let bond = bond.ok_or_else(|| "reserved one-ZEC Names bond is unavailable".to_string())?;
    let funding = funding
        .ok_or_else(|| "wallet needs a separate confirmed Ironwood note for the fee".to_string())?;
    let bond_nf = bond.note().nullifier(&wallet_fvk).to_bytes();
    let funding_nf = funding.note().nullifier(&wallet_fvk).to_bytes();

    let (prover, _) = keygen();
    let prepared = prepare_reveal(
        RevealInputs {
            wallet_seed: seed.expose_secret(),
            deployment: context.deployment,
            name: name.clone(),
            commit_ref: commit,
            ua,
            operation_height,
            designated_action_index: 0,
            registration_fvk: &wallet_fvk,
            registration_note: *bond.note(),
        },
        &prover,
        rand_10::rng(),
    )
    .map_err(|error| format!("prepare Names REVEAL: {error:#}"))?;
    drop(seed);

    let carrier_count = prepared.publication().frames().len();
    let shape = names_ironwood_shape_from_counts(2, carrier_count, 1, 0)
        .map_err(|error| format!("plan Names REVEAL shape: {error:#}"))?;
    let fee =
        required_zip317_fee_for_names(&network, BlockHeight::from_u32(operation_height), shape)
            .map_err(|error| format!("plan Names REVEAL fee: {error:#}"))?;
    let change_value = funding_value
        .checked_sub(fee.into_u64())
        .ok_or_else(|| "separate Ironwood note cannot cover Names fee".to_string())?;
    let funding_ref = OutputRef::new(
        *funding.txid(),
        PoolType::Shielded(ShieldedPool::Ironwood),
        u32::from(funding.output_index()),
    );
    let funding_owner = LockOwner::new(registration.commitment);
    with_wallet_db_write_lock("names.reserve_reveal_fee_note", || {
        db.lock_outputs(
            &[funding_ref],
            funding_owner,
            BlockHeight::from_u32(expiry_height),
        )
        .map_err(|error| format!("reserve Names REVEAL fee note: {error:?}"))
    })?;
    let fee_reservation = NamesFeeReservation {
        db_path: db_path.into(),
        network,
        output: Some(funding_ref),
        owner: funding_owner,
    };

    let plan = prepared
        .ironwood_plan(
            wallet_fvk.clone(),
            *bond.note(),
            vec![FundingSpend {
                fvk: wallet_fvk.clone(),
                note: *funding.note(),
            }],
            vec![ChangeOutput {
                fvk: wallet_fvk.clone(),
                ovk: None,
                recipient: wallet_fvk.address_at(0u32, Scope::Internal),
                value: NoteValue::from_raw(change_value),
                memo: [0; 512],
            }],
        )
        .map_err(|error| format!("plan Names REVEAL: {error:#}"))?;
    let built = build_names_bundle(plan, rand_10::rng())
        .map_err(|error| format!("build Names REVEAL bundle: {error:#}"))?;
    let built = build_names_pczt(NamesPcztPlan {
        ironwood: built,
        params: network,
        consensus_branch_id: BranchId::Nu6_3,
        expiry_height: BlockHeight::from_u32(expiry_height),
        fallback_lock_time: 0,
    })
    .map_err(|error| format!("build Names REVEAL PCZT: {error:#}"))?;
    let finalized = finalize_names_pczt_io(built)
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
                bond.note_commitment_tree_position(),
                funding.note_commitment_tree_position(),
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
    let [path0, path1] = paths.map(|path| {
        path.map_err(|error| format!("read Names witness: {error:?}"))?
            .ok_or_else(|| "wallet has no Ironwood witness at anchor".to_string())
            .map(Into::into)
    });
    let witnessed = install_names_ironwood_witnesses(
        finalized,
        NamesWitnessPlan {
            anchor,
            spends: vec![
                NamesIronwoodWitness {
                    nullifier: bond_nf,
                    merkle_path: path0?,
                },
                NamesIronwoodWitness {
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
    let proved = prove_names_ironwood_pczt(witnessed, &consensus_key)
        .map_err(|error| format!("prove Names transaction: {error:#}"))?;
    let signed = sign_names_ironwood_pczt(
        proved,
        NamesSigningPlan {
            spends: vec![
                NamesIronwoodSigningKey {
                    nullifier: bond_nf,
                    ask: wallet_ask.clone(),
                },
                NamesIronwoodSigningKey {
                    nullifier: funding_nf,
                    ask: wallet_ask,
                },
            ],
        },
    )
    .map_err(|error| format!("sign Names REVEAL: {error:#}"))?;
    let extracted = extract_names_transaction(signed)
        .map_err(|error| format!("extract Names REVEAL: {error:#}"))?;
    let txid = extracted.txid.into();
    let mut raw = Vec::new();
    extracted
        .transaction
        .write(&mut raw)
        .map_err(|error| format!("encode Names REVEAL: {error}"))?;
    Ok(NamesTransaction {
        raw,
        txid,
        name: registration.name,
        account_uuid: account_uuid.into(),
        db_path: db_path.into(),
        network,
        valid_from_height: operation_height,
        expiry_height,
        fee_zatoshi: fee.into_u64(),
        fee_reservation: Some(fee_reservation),
    })
}

fn store_reviewed_reveal_capability(
    transaction: NamesTransaction,
    send_flow_id: &str,
) -> Result<u64, String> {
    let reservation = Arc::new(Mutex::new(transaction.fee_reservation));
    let release_reservation = Arc::clone(&reservation);
    let release = Arc::new(move || {
        let mut reservation = release_reservation
            .lock()
            .map_err(|error| format!("lock Names REVEAL fee reservation: {error}"))?;
        let _ = reservation.take();
        Ok(())
    });
    let retain_reservation = reservation;
    let retain = Arc::new(move |_lock: sync::NamesRevealLockMetadata| {
        let mut reservation = retain_reservation
            .lock()
            .map_err(|error| format!("lock Names REVEAL fee reservation: {error}"))?;
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
    ensure_live_tip(lightwalletd_url, context.tip_height).await?;
    let transaction = build_reveal(db_path, network, account_uuid, name, seed)?;
    ensure_transaction_window_open(
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

pub(crate) async fn reveal_registration(
    db_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    seed: SecretVec<u8>,
) -> Result<[u8; 32], String> {
    let context = coppice::lifecycle_context(db_path, network)?;
    ensure_live_tip(lightwalletd_url, context.tip_height).await?;
    let mut transaction = build_reveal(db_path, network, account_uuid, name, seed)?;
    ensure_transaction_window_open(
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

async fn ensure_live_tip(lightwalletd_url: &str, expected: u32) -> Result<(), String> {
    let mut client = super::sync_engine::open_isolated_lwd_channel(lightwalletd_url)
        .await
        .map_err(|error| format!("open chain-tip channel: {error}"))?;
    let tip = super::sync_engine::get_latest_block(&mut client)
        .await
        .map_err(|error| format!("read chain tip: {error}"))?;
    let actual =
        u32::try_from(tip.height).map_err(|_| "chain tip exceeds supported height".to_string())?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "wallet Names state is at {expected}, chain is at {actual}; sync before constructing"
        ))
    }
}

pub(crate) async fn ensure_transaction_window_open(
    lightwalletd_url: &str,
    valid_from_height: u32,
    expiry_height: u32,
) -> Result<(), String> {
    let mut client = super::sync_engine::open_isolated_lwd_channel(lightwalletd_url)
        .await
        .map_err(|error| format!("open chain-tip channel: {error}"))?;
    let tip = super::sync_engine::get_latest_block(&mut client)
        .await
        .map_err(|error| format!("read chain tip: {error}"))?;
    let next = u32::try_from(tip.height)
        .ok()
        .and_then(|height| height.checked_add(1))
        .ok_or_else(|| "chain height overflow".to_string())?;
    if (valid_from_height..=expiry_height).contains(&next) {
        Ok(())
    } else {
        Err(format!(
            "Names transaction is valid at {valid_from_height}..={expiry_height}; next block is {next}"
        ))
    }
}

fn recover_current_bond(
    seed: &[u8],
    deployment: coppice_names::deployment::DeploymentParameters,
    network: coppice_names::protocol::Network,
    name: &Name,
    origin: BondOrigin,
) -> Result<(FullViewingKey, SpendAuthorizingKey, orchard::note::Note), String> {
    let deployment_id = deployment
        .deployment_id()
        .map_err(|error| format!("derive Names deployment ID: {error:?}"))?;
    let spending_key = derive_name_spending_key(seed, deployment_id, name)
        .map_err(|error| format!("derive per-name authority: {error:?}"))?;
    let fvk = FullViewingKey::from(&spending_key);
    let ask = SpendAuthorizingKey::from(&spending_key);
    let note = match origin {
        BondOrigin::Reveal {
            commit,
            epoch,
            ua,
            action_index,
            action_nullifier,
        } => derive_reveal_bond_note(
            &spending_key,
            deployment_id,
            commit,
            epoch,
            &CanonicalUa::parse(network, &ua)
                .map_err(|error| format!("stored bond UA is invalid: {error:?}"))?,
            action_index,
            FieldElement::from_bytes(action_nullifier)
                .map_err(|error| format!("stored bond nullifier is invalid: {error:?}"))?,
        ),
        BondOrigin::Refresh {
            predecessor,
            epoch,
            ua,
            action_index,
            action_nullifier,
        } => derive_refresh_bond_note(
            &spending_key,
            deployment_id,
            predecessor,
            epoch,
            &CanonicalUa::parse(network, &ua)
                .map_err(|error| format!("stored bond UA is invalid: {error:?}"))?,
            action_index,
            FieldElement::from_bytes(action_nullifier)
                .map_err(|error| format!("stored bond nullifier is invalid: {error:?}"))?,
        ),
    }
    .map_err(|error| format!("reconstruct current Names bond: {error:?}"))?;
    Ok((fvk, ask, note))
}

#[derive(Clone, Debug)]
pub(crate) enum NamesTransitionKind {
    Update(String),
    Renew,
    Release,
}

fn build_refresh(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    requested_ua: Option<&str>,
    seed: SecretVec<u8>,
) -> Result<NamesTransaction, String> {
    let name = canonical_registration_name(name)?;
    let context = coppice::lifecycle_context(db_path, network)?;
    let resolution = coppice::accepted_managed_resolution(db_path, network, name.as_str())?
        .ok_or_else(|| "the name is not tracked by this wallet".to_string())?;
    let predecessor = resolution
        .head
        .ok_or_else(|| "the name has no accepted canonical head".to_string())?;
    let origin = resolution
        .bond_origin
        .ok_or_else(|| "current Names bond recovery metadata is unavailable".to_string())?;
    let position = resolution
        .marked_position
        .ok_or_else(|| "current Names bond witness position is unavailable".to_string())?;
    let operation_height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names construction height overflow".to_string())?;
    let name_id = name
        .id()
        .map_err(|error| format!("derive name ID: {error:?}"))?;
    if predecessor.lifecycle(operation_height, context.parameters)
        != coppice_names::reducer::Lifecycle::Active
        || !context
            .parameters
            .accepts_operation(name_id, operation_height)
    {
        return Err("REFRESH must wait for the deterministic name window".into());
    }
    let epoch = context
        .parameters
        .epoch(operation_height)
        .map_err(|error| format!("derive Names epoch: {error:?}"))?;
    let window = context
        .parameters
        .window(name_id, epoch)
        .map_err(|error| format!("derive Names window: {error:?}"))?;
    let ua = CanonicalUa::parse(
        context.network,
        requested_ua.unwrap_or_else(|| predecessor.ua.as_str()),
    )
    .map_err(|error| format!("invalid canonical Unified Address: {error:?}"))?;
    let (name_fvk, name_ask, predecessor_note) = recover_current_bond(
        seed.expose_secret(),
        context.deployment,
        context.network,
        &name,
        origin,
    )?;

    let account_id = parse_account_uuid(account_uuid)?;
    let mut db = open_wallet_db(db_path, network)?;
    let account = db
        .get_account(account_id)
        .map_err(|error| format!("read Names account: {error}"))?
        .ok_or_else(|| "Names account not found".to_string())?;
    let account_index = account
        .source()
        .key_derivation()
        .ok_or_else(|| "Names requires a software-derived account".to_string())?
        .account_index();
    let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), account_index)
        .map_err(|error| format!("derive wallet spending key: {error:?}"))?;
    let wallet_fvk = FullViewingKey::from(usk.orchard());
    let wallet_ask = SpendAuthorizingKey::from(usk.orchard());
    let funding = db
        .get_unspent_ironwood_notes_at_historical_height(
            account_id,
            BlockHeight::from_u32(context.tip_height),
        )
        .map_err(|error| format!("read Names fee notes: {error}"))?
        .into_iter()
        .filter_map(|candidate| {
            db.get_spendable_note(
                candidate.txid(),
                ShieldedPool::Ironwood,
                u32::from(candidate.output_index()),
                TargetHeight::from(BlockHeight::from_u32(operation_height)),
                LockFilter::Policy(&LockedInputPolicy::Exclude),
            )
            .ok()
            .flatten()
            .map(|_| candidate)
        })
        .max_by_key(|candidate| candidate.note().value().inner())
        .ok_or_else(|| "wallet needs a confirmed Ironwood note for the fee".to_string())?;
    let predecessor_nf = predecessor_note.nullifier(&name_fvk).to_bytes();
    let funding_nf = funding.note().nullifier(&wallet_fvk).to_bytes();
    let (prover, _) = keygen();
    let prepared = prepare_refresh(
        RefreshInputs {
            wallet_seed: seed.expose_secret(),
            deployment: context.deployment,
            name: name.clone(),
            predecessor: predecessor.clone(),
            predecessor_note,
            ua,
            operation_height,
            designated_action_index: 0,
        },
        &prover,
        rand_10::rng(),
    )
    .map_err(|error| format!("prepare Names REFRESH: {error:#}"))?;
    drop(seed);

    let shape = names_ironwood_shape_from_counts(2, prepared.publication().frames().len(), 1, 0)
        .map_err(|error| format!("plan Names REFRESH shape: {error:#}"))?;
    let fee =
        required_zip317_fee_for_names(&network, BlockHeight::from_u32(operation_height), shape)
            .map_err(|error| format!("plan Names REFRESH fee: {error:#}"))?;
    let change_value = funding
        .note()
        .value()
        .inner()
        .checked_sub(fee.into_u64())
        .ok_or_else(|| "Ironwood note cannot cover Names fee".to_string())?;
    let funding_ref = OutputRef::new(
        *funding.txid(),
        PoolType::Shielded(ShieldedPool::Ironwood),
        u32::from(funding.output_index()),
    );
    let lock_owner = LockOwner::new(predecessor.commitment.to_bytes());
    with_wallet_db_write_lock("names.reserve_refresh_fee_note", || {
        db.lock_outputs(
            &[funding_ref],
            lock_owner,
            BlockHeight::from_u32(window.end.saturating_sub(1)),
        )
        .map_err(|error| format!("reserve Names REFRESH fee note: {error:?}"))
    })?;
    let fee_reservation = NamesFeeReservation {
        db_path: db_path.into(),
        network,
        output: Some(funding_ref),
        owner: lock_owner,
    };
    let plan = prepared
        .ironwood_plan(
            name_fvk,
            predecessor_note,
            vec![FundingSpend {
                fvk: wallet_fvk.clone(),
                note: *funding.note(),
            }],
            vec![ChangeOutput {
                fvk: wallet_fvk.clone(),
                ovk: None,
                recipient: wallet_fvk.address_at(0u32, Scope::Internal),
                value: NoteValue::from_raw(change_value),
                memo: [0; 512],
            }],
        )
        .map_err(|error| format!("plan Names REFRESH: {error:#}"))?;
    let built = build_names_bundle(plan, rand_10::rng())
        .map_err(|error| format!("build Names REFRESH bundle: {error:#}"))?;
    let built = build_names_pczt(NamesPcztPlan {
        ironwood: built,
        params: network,
        consensus_branch_id: BranchId::Nu6_3,
        expiry_height: BlockHeight::from_u32(window.end.saturating_sub(1)),
        fallback_lock_time: 0,
    })
    .map_err(|error| format!("build Names REFRESH PCZT: {error:#}"))?;
    let finalized = finalize_names_pczt_io(built)
        .map_err(|error| format!("finalize Names REFRESH PCZT: {error:#}"))?;
    let anchor_height = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .map_err(|error| format!("read Names anchor: {error}"))?
        .ok_or_else(|| "wallet has no Names anchor height".to_string())?
        .1;
    let (anchor, paths) = with_wallet_db_write_lock("names.read_refresh_witnesses", || {
        db.with_ironwood_tree_mut::<_, _, SqliteClientError>(|tree| {
            let anchor = tree.root_at_checkpoint_id(&anchor_height)?;
            let paths = [
                incrementalmerkletree::Position::from(u64::from(position)),
                funding.note_commitment_tree_position(),
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
    let [path0, path1] = paths.map(|path| {
        path.map_err(|error| format!("read Names witness: {error:?}"))?
            .ok_or_else(|| "wallet has no Ironwood witness at anchor".to_string())
            .map(Into::into)
    });
    let witnessed = install_names_ironwood_witnesses(
        finalized,
        NamesWitnessPlan {
            anchor,
            spends: vec![
                NamesIronwoodWitness {
                    nullifier: predecessor_nf,
                    merkle_path: path0?,
                },
                NamesIronwoodWitness {
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
    let proved = prove_names_ironwood_pczt(witnessed, &consensus_key)
        .map_err(|error| format!("prove Names transaction: {error:#}"))?;
    let signed = sign_names_ironwood_pczt(
        proved,
        NamesSigningPlan {
            spends: vec![
                NamesIronwoodSigningKey {
                    nullifier: predecessor_nf,
                    ask: name_ask,
                },
                NamesIronwoodSigningKey {
                    nullifier: funding_nf,
                    ask: wallet_ask,
                },
            ],
        },
    )
    .map_err(|error| format!("sign Names REFRESH: {error:#}"))?;
    let extracted = extract_names_transaction(signed)
        .map_err(|error| format!("extract Names REFRESH: {error:#}"))?;
    let txid = extracted.txid.into();
    let mut raw = Vec::new();
    extracted
        .transaction
        .write(&mut raw)
        .map_err(|error| format!("encode Names REFRESH: {error}"))?;
    Ok(NamesTransaction {
        raw,
        txid,
        name: name.as_str().into(),
        account_uuid: account_uuid.into(),
        db_path: db_path.into(),
        network,
        valid_from_height: operation_height,
        expiry_height: window.end.saturating_sub(1),
        fee_zatoshi: fee.into_u64(),
        fee_reservation: Some(fee_reservation),
    })
}

fn build_release(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    name: &str,
    seed: SecretVec<u8>,
) -> Result<NamesTransaction, String> {
    const RELEASE_EXPIRY_BLOCKS: u32 = 40;

    let name = canonical_registration_name(name)?;
    let context = coppice::lifecycle_context(db_path, network)?;
    let resolution = coppice::accepted_managed_resolution(db_path, network, name.as_str())?
        .ok_or_else(|| "the name is not tracked by this wallet".to_string())?;
    let _predecessor = resolution
        .head
        .ok_or_else(|| "the name has no accepted canonical head".to_string())?;
    let origin = resolution
        .bond_origin
        .ok_or_else(|| "current Names bond recovery metadata is unavailable".to_string())?;
    let position = resolution
        .marked_position
        .ok_or_else(|| "current Names bond witness position is unavailable".to_string())?;
    let target_height = context
        .tip_height
        .checked_add(1)
        .ok_or_else(|| "Names RELEASE target height overflow".to_string())?;
    let expiry_height = target_height
        .checked_add(RELEASE_EXPIRY_BLOCKS)
        .ok_or_else(|| "Names RELEASE expiry height overflow".to_string())?;
    let (name_fvk, name_ask, bond_note) = recover_current_bond(
        seed.expose_secret(),
        context.deployment,
        context.network,
        &name,
        origin,
    )?;

    let account_id = parse_account_uuid(account_uuid)?;
    let mut db = open_wallet_db(db_path, network)?;
    let account = db
        .get_account(account_id)
        .map_err(|error| format!("read Names account: {error}"))?
        .ok_or_else(|| "Names account not found".to_string())?;
    let account_index = account
        .source()
        .key_derivation()
        .ok_or_else(|| "Names requires a software-derived account".to_string())?
        .account_index();
    let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), account_index)
        .map_err(|error| format!("derive wallet spending key: {error:?}"))?;
    let wallet_fvk = FullViewingKey::from(usk.orchard());
    drop(seed);

    let shape = names_ironwood_shape_from_counts(1, 0, 0, 0)
        .map_err(|error| format!("plan Names RELEASE shape: {error:#}"))?;
    let fee = required_zip317_fee_for_names(&network, BlockHeight::from_u32(target_height), shape)
        .map_err(|error| format!("plan Names RELEASE fee: {error:#}"))?;
    let output_value = bond_note
        .value()
        .inner()
        .checked_sub(fee.into_u64())
        .ok_or_else(|| "Names bond cannot cover the RELEASE fee".to_string())?;
    let anchor_height = db
        .get_target_and_anchor_heights(std::num::NonZeroU32::MIN)
        .map_err(|error| format!("read Names RELEASE anchor: {error}"))?
        .ok_or_else(|| "wallet has no Names RELEASE anchor height".to_string())?
        .1;
    let (anchor, path) = with_wallet_db_write_lock("names.read_release_witness", || {
        db.with_ironwood_tree_mut::<_, _, SqliteClientError>(|tree| {
            let anchor = tree.root_at_checkpoint_id(&anchor_height)?;
            let path = tree.witness_at_checkpoint_id_caching(
                incrementalmerkletree::Position::from(u64::from(position)),
                &anchor_height,
            )?;
            Ok((anchor, path))
        })
    })
    .map_err(|error| format!("read Names RELEASE witness: {error}"))?
    .ok_or_else(|| "wallet has no Ironwood commitment tree".to_string())?;
    let anchor = anchor
        .ok_or_else(|| "wallet has no Ironwood anchor root".to_string())?
        .into();
    let path = path
        .ok_or_else(|| "wallet has no Ironwood bond witness at anchor".to_string())?
        .into();
    let consensus_key = orchard::circuit::ProvingKey::build(
        orchard::bundle::BundleVersion::ironwood_v3().circuit_version(),
    );
    let extracted = build_ordinary_ironwood_spend(
        OrdinaryIronwoodSpendPlan {
            params: network,
            consensus_branch_id: BranchId::Nu6_3,
            expiry_height: BlockHeight::from_u32(expiry_height),
            fallback_lock_time: 0,
            anchor,
            input_fvk: name_fvk,
            input_note: bond_note,
            input_witness: path,
            input_ask: name_ask,
            output: ChangeOutput {
                fvk: wallet_fvk.clone(),
                ovk: Some(wallet_fvk.to_ovk(Scope::Internal)),
                recipient: wallet_fvk.address_at(0u32, Scope::Internal),
                value: NoteValue::from_raw(output_value),
                memo: [0; 512],
            },
        },
        &consensus_key,
        rand_10::rng(),
    )
    .map_err(|error| format!("build Names RELEASE: {error:#}"))?;
    let txid = extracted.txid.into();
    let mut raw = Vec::new();
    extracted
        .transaction
        .write(&mut raw)
        .map_err(|error| format!("encode Names RELEASE: {error}"))?;
    Ok(NamesTransaction {
        raw,
        txid,
        name: name.as_str().into(),
        account_uuid: account_uuid.into(),
        db_path: db_path.into(),
        network,
        valid_from_height: target_height,
        expiry_height,
        fee_zatoshi: extracted.fee_zatoshi,
        fee_reservation: None,
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
    let context = coppice::lifecycle_context(db_path, network)?;
    ensure_live_tip(lightwalletd_url, context.tip_height).await?;
    let mut transaction = match &kind {
        NamesTransitionKind::Update(address) => build_refresh(
            db_path,
            network,
            account_uuid,
            name,
            Some(address.as_str()),
            seed,
        )?,
        NamesTransitionKind::Renew => {
            build_refresh(db_path, network, account_uuid, name, None, seed)?
        }
        NamesTransitionKind::Release => build_release(db_path, network, account_uuid, name, seed)?,
    };
    ensure_transaction_window_open(
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
    use super::*;

    #[test]
    fn canonical_name_accepts_one_suffix() {
        assert_eq!(
            canonical_registration_name("Alice.zec").unwrap().as_str(),
            "alice"
        );
        assert!(canonical_registration_name("alice.example").is_err());
    }

    #[test]
    fn draft_schedule_selects_a_real_future_window() {
        let parameters = coppice_names::schedule::Parameters::candidate([9; 32], 100);
        let name = Name::parse("alice").unwrap();
        for next_height in [100, 333, 1_251, 8_000] {
            let (epoch, reveal) = target_reveal(parameters, &name, next_height).unwrap();
            let window = parameters.window(name.id().unwrap(), epoch).unwrap();
            assert!(window.contains(reveal));
            assert!(reveal >= next_height + parameters.commit_maturity_blocks);
        }
    }
}
