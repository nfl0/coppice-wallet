//! Live regtest regression for the wallet Names registration lifecycle:
//! funding -> configure/bootstrap -> draft -> COMMIT -> REVEAL at the
//! scheduled anchor -> Active -> resolution. Drives the same public API
//! entrypoints the Flutter wallet calls, so a failure here reproduces
//! user-visible behavior.

mod common;

use common::{
    create_wallet, exclusive_regtest, path_str, sync_wallet, LIGHTWALLETD_URL, REGTEST_NETWORK,
};
use rust_lib_zcash_wallet::api::{names as names_api, sync as sync_api};

/// The current regtest stack lives in `regtest-dev/` (Zakura + Zaino), not in
/// the legacy docker scripts, so funding and mining go through its driver.
fn devscript(args: &[&str]) {
    let script = common::repo_root()
        .parent()
        .expect("coppice workspace root")
        .join("regtest-dev/regtest-dev.sh");
    let output = std::process::Command::new(script)
        .args(args)
        .output()
        .expect("run regtest-dev.sh");
    if !output.status.success() {
        panic!(
            "regtest-dev.sh {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// Funds from the stack faucet with 10 confirmation blocks.
fn fund(address: &str, amount: &str) {
    devscript(&["fund", address, amount, "10"]);
}

fn mine(blocks: u32) {
    devscript(&["mine", &blocks.to_string()]);
}

const REGTEST_IVK: &str = "65deb2b3ee7ac69020543f40f21122cb6dc1f4201a329fcdf9d5e3bb2dfbbabe\
     29d542352fe36c3c7b24c2989dc9d0000b9e04f444e05dc4538bde395c0e6008";
const REGTEST_RECEIVER: &str = "9ec59e4d447ba285086cc3456cadf62004a19b6a7989c726daaa9944a6cdbf25\
     f7bfa51afa15b66da53881";

/// Unique per run: a fixed name could not be re-registered after a previous
/// test run revealed it (the lease holds until expiry).
fn unique_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    format!("life{millis}")
}

fn managed_snapshot(
    db_path: &str,
    account_uuid: &str,
    name: &str,
) -> rust_lib_zcash_wallet::api::names::ApiManagedName {
    names_api::get_managed_names_v1(
        db_path.to_string(),
        REGTEST_NETWORK.to_string(),
        account_uuid.to_string(),
    )
    .expect("get_managed_names_v1")
    .into_iter()
    .find(|entry| entry.name == name)
    .expect("registration workflow present")
}

#[test]
#[ignore = "requires and mutates the live Zakura/Zaino regtest stack"]
fn names_commit_reveal_live() {
    let _guard = exclusive_regtest();

    // Align consensus params with the local stack (Ironwood/NU6.3 active at
    // height 2). Without this the wallet treats Ironwood notes as never
    // spendable; the app performs the same configuration at startup.
    rust_lib_zcash_wallet::api::simple::configure_regtest_ironwood_activation_height(2)
        .expect("configure regtest Ironwood activation");

    let (tempdir, wallet) = create_wallet("names-lifecycle");
    let db_file = tempdir.path().join("zcash_wallet.db");
    let db_path = path_str(&db_file);
    let mnemonic = wallet.mnemonic.clone();
    let account_uuid = wallet.account_uuid.clone();
    let name = unique_name();

    // Exact 1 ZEC Ironwood note for the bond, plus fee/change funds. Each
    // fund mines 10 confirmation blocks.
    fund(&wallet.unified_address, "1");
    fund(&wallet.unified_address, "2");
    sync_wallet(&db_file);

    names_api::configure_names_v1(
        db_path.clone(),
        REGTEST_NETWORK.to_string(),
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
        "coppice-runtime-regtest-v1".to_string(),
        REGTEST_IVK.to_string(),
        REGTEST_RECEIVER.to_string(),
    )
    .expect("configure Names");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(names_api::bootstrap_names_v1(
            db_path.clone(),
            LIGHTWALLETD_URL.to_string(),
            REGTEST_NETWORK.to_string(),
        ))
        .expect("bootstrap Names");

    names_api::prepare_names_v1_registration_draft(
        db_path.clone(),
        REGTEST_NETWORK.to_string(),
        account_uuid.clone(),
        name.clone(),
        wallet.unified_address.clone(),
        mnemonic.as_bytes().to_vec(),
    )
    .expect("prepare registration draft");

    let send_flow_id = "names-lifecycle-flow";
    let proposal = names_api::begin_names_v1_registration(
        db_path.clone(),
        REGTEST_NETWORK.to_string(),
        account_uuid.clone(),
        send_flow_id.to_string(),
        name.clone(),
        wallet.unified_address.clone(),
        mnemonic.as_bytes().to_vec(),
    )
    .expect("begin registration (COMMIT proposal)");

    sync_api::execute_proposal(
        db_path.clone(),
        LIGHTWALLETD_URL.to_string(),
        proposal.proposal_id,
        send_flow_id.to_string(),
        mnemonic.as_bytes().to_vec(),
        None,
        None,
    )
    .expect("execute COMMIT proposal");

    // Mine and sync until canonical replay accepts the COMMIT.
    let mut accepted = None;
    for _ in 0..20 {
        mine(1);
        sync_wallet(&db_file);
        let snapshot = managed_snapshot(&db_path, &account_uuid, &name);
        if snapshot.phase == "commit_accepted" {
            accepted = Some(snapshot);
            break;
        }
    }
    accepted.expect("COMMIT to be accepted by canonical replay");

    // Advance to the name's scheduled anchor (the UI enables REVEAL only
    // when the next block is exactly the anchor).
    let mut reveal_view = managed_snapshot(&db_path, &account_uuid, &name);
    for _ in 0..20 {
        if reveal_view.reveal_ready {
            break;
        }
        mine(1);
        sync_wallet(&db_file);
        reveal_view = managed_snapshot(&db_path, &account_uuid, &name);
    }
    assert!(
        reveal_view.reveal_ready,
        "REVEAL anchor reached within 20 blocks"
    );

    // The exact call the wallet UI makes when the user presses "Reveal now".
    let reveal_txid = names_api::reveal_names_v1_registration(
        db_path.clone(),
        LIGHTWALLETD_URL.to_string(),
        REGTEST_NETWORK.to_string(),
        account_uuid.clone(),
        name.clone(),
        mnemonic.as_bytes().to_vec(),
    )
    .expect("reveal at the scheduled anchor");

    mine(1);
    sync_wallet(&db_file);
    let final_state = managed_snapshot(&db_path, &account_uuid, &name);
    assert_eq!(
        final_state.phase, "active",
        "name should be active after REVEAL"
    );

    // The send flow's resolution boundary: only `active` yields a payment
    // address, and it must be the registered one.
    let resolution = runtime
        .block_on(names_api::resolve_name_v1(
            db_path.clone(),
            LIGHTWALLETD_URL.to_string(),
            REGTEST_NETWORK.to_string(),
            format!("{name}.zec"),
        ))
        .expect("resolve the active name");
    assert_eq!(resolution.status, "active");
    assert_eq!(
        resolution.payment_address.expect("payment address"),
        wallet.unified_address,
        "resolution should return the registered payment address"
    );

    // A REVEAL outside the scheduled window must be rejected; its error is
    // user-facing copy that names the terminal state.
    let late_error = names_api::reveal_names_v1_registration(
        db_path.clone(),
        LIGHTWALLETD_URL.to_string(),
        REGTEST_NETWORK.to_string(),
        account_uuid.clone(),
        name.clone(),
        mnemonic.as_bytes().to_vec(),
    )
    .expect_err("REVEAL after the name is active must be rejected");
    assert!(
        late_error.contains("no pending registration")
            || late_error.contains("not yet accepted")
            || late_error.contains("scheduled block"),
        "unexpected late-REVEAL error: {late_error}"
    );

    let _ = reveal_txid;
}
