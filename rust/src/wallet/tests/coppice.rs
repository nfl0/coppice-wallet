use super::super::network::WalletNetwork;
use super::{
    bootstrap, compact_tx_is_rendezvous, configure, contiguous_ranges, read_stored, resolve_name,
    sidecar_path, write_stored, AcquiredCanonicalSource, NamesWalletConfig, StoredNamesWallet,
};
use coppice_names::v1::{CanonicalBlock, CanonicalSource};
use std::collections::BTreeMap;
use zcash_client_backend::proto::compact_formats::CompactTx;

const IVK_HEX: &str =
    "65deb2b3ee7ac69020543f40f21122cb6dc1f4201a329fcdf9d5e3bb2dfbbabe29d542352fe36c3c7b24c2989dc9d0000b9e04f444e05dc4538bde395c0e6008";
const RECEIVER_HEX: &str =
    "9ec59e4d447ba285086cc3456cadf62004a19b6a7989c726daaa9944a6cdbf25f7bfa51afa15b66da53881";

fn config() -> NamesWalletConfig {
    NamesWalletConfig::from_api(
        WalletNetwork::Regtest,
        10,
        10,
        8,
        15,
        16,
        32,
        3,
        4,
        1024,
        1,
        64,
        "coppice-runtime-regtest-v1".into(),
        IVK_HEX.into(),
        RECEIVER_HEX.into(),
    )
    .unwrap()
}

#[test]
fn config_requires_runtime_before_names_and_exact_rendezvous_material() {
    let mut invalid = config();
    invalid.network_code = 1;
    assert!(invalid
        .validated_core_parameters(WalletNetwork::Regtest)
        .is_err());

    assert!(NamesWalletConfig::from_api(
        WalletNetwork::Regtest,
        10,
        9,
        8,
        15,
        16,
        32,
        3,
        4,
        1024,
        1,
        64,
        "coppice-runtime-regtest-v1".into(),
        IVK_HEX.into(),
        RECEIVER_HEX.into(),
    )
    .is_err());

    assert!(NamesWalletConfig::from_api(
        WalletNetwork::Regtest,
        10,
        10,
        8,
        15,
        16,
        32,
        3,
        4,
        1024,
        1,
        64,
        "coppice-runtime-regtest-v1".into(),
        "00".into(),
        RECEIVER_HEX.into(),
    )
    .is_err());
}

#[test]
fn sidecar_roundtrip_is_atomic_and_rejects_oversized_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("wallet.sqlite");
    let sidecar = sidecar_path(db_path.to_str().unwrap());
    let stored = StoredNamesWallet::configured(config());
    write_stored(&sidecar, &stored).unwrap();
    assert_eq!(
        read_stored(&sidecar).unwrap().unwrap().config,
        stored.config
    );

    let oversized = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&sidecar)
        .unwrap();
    oversized.set_len(65 * 1024 * 1024).unwrap();
    assert!(read_stored(&sidecar).is_err());
}

#[test]
fn malformed_compact_action_is_rejected_before_full_transaction_fetch() {
    let parameters = config()
        .validated_core_parameters(WalletNetwork::Regtest)
        .unwrap();
    let rendezvous = coppice::carrier::CoreRendezvous::from_validated(&parameters);
    let tx = CompactTx {
        ironwood_actions: vec![Default::default()],
        ..Default::default()
    };
    assert!(compact_tx_is_rendezvous(&tx, &rendezvous).is_err());
}

fn block(height: u32, hash: u8, previous_hash: u8) -> CanonicalBlock {
    CanonicalBlock {
        height,
        block_hash: [hash; 32],
        prev_block_hash: [previous_hash; 32],
        transactions: Vec::new(),
    }
}

#[test]
fn lineage_source_records_exact_gaps_and_rejects_forked_extensions() {
    let mut tail = BTreeMap::new();
    tail.insert(10, block(10, 10, 9));
    tail.insert(11, block(11, 11, 10));
    let mut source = AcquiredCanonicalSource::new(tail, 11).unwrap();

    assert!(source.block(9).is_none());
    assert_eq!(source.take_missing().into_iter().collect::<Vec<_>>(), [9]);

    let mut predecessor = BTreeMap::new();
    predecessor.insert(9, block(9, 9, 8));
    source
        .extend(AcquiredCanonicalSource::new(predecessor, 9).unwrap())
        .unwrap();
    assert_eq!(source.block(9).unwrap().block_hash, [9; 32]);

    let mut fork = BTreeMap::new();
    fork.insert(8, block(8, 8, 7));
    fork.insert(9, block(9, 99, 8));
    assert!(source
        .extend(AcquiredCanonicalSource::new(fork, 9).unwrap())
        .is_err());
    assert!(!source.contains(8));
}

#[test]
fn missing_lineage_heights_are_grouped_into_minimal_ranges() {
    assert_eq!(
        contiguous_ranges(&[2, 3, 4, 8, 10, 11]),
        vec![(2, 4), (8, 8), (10, 11)]
    );
}

/// Opt-in smoke test for the real Zakura/Zaino regtest stack. It deliberately
/// uses an empty Names history: lifecycle proving remains in the dedicated
/// qualification harness, while this test exercises the wallet's actual gRPC
/// acquisition, authenticated activation checkpoint, pre-bootstrap bounded
/// FreshResolver path, and durable full-state bootstrap.
#[tokio::test]
#[ignore = "requires a live Zakura/Zaino regtest endpoint"]
async fn live_zaino_bootstrap_and_missing_resolution() {
    let lightwalletd_url = std::env::var("COPPICE_NAMES_TEST_LIGHTWALLETD")
        .unwrap_or_else(|_| "http://127.0.0.1:8137".into());
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("wallet.sqlite");
    let db_path = db_path.to_str().unwrap();

    let configured = configure(
        db_path,
        WalletNetwork::Regtest,
        2,
        2,
        8,
        15,
        16,
        32,
        3,
        4,
        1024,
        1,
        128,
        "coppice-runtime-regtest-v1".into(),
        IVK_HEX.into(),
        RECEIVER_HEX.into(),
    )
    .unwrap();
    assert_eq!(configured.state, "needs_bootstrap");

    let prebootstrap_resolution = resolve_name(
        db_path,
        &lightwalletd_url,
        WalletNetwork::Regtest,
        "coppice-wallet-smoke",
    )
    .await
    .unwrap();
    assert_eq!(prebootstrap_resolution.status, "missing");

    let ready = bootstrap(db_path, &lightwalletd_url, WalletNetwork::Regtest)
        .await
        .unwrap();
    assert_eq!(ready.state, "ready");
    assert!(ready.tip_height >= 2);

    let resolution = resolve_name(
        db_path,
        &lightwalletd_url,
        WalletNetwork::Regtest,
        "coppice-wallet-smoke",
    )
    .await
    .unwrap();
    assert_eq!(resolution.status, "missing");
    assert_eq!(resolution.tip_height, ready.tip_height);
    assert_eq!(resolution.tip_height, prebootstrap_resolution.tip_height);
}
