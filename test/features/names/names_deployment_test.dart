import 'package:flutter_test/flutter_test.dart';
import 'package:zcash_wallet/src/features/names/models/names_deployment.dart';

void main() {
  test('mainnet and testnet carry no deployment profile', () {
    // The explicitness contract: no public Coppice/Names deployment exists,
    // so the UI must never find regtest material for production networks.
    expect(namesDeploymentProfileForNetwork('main'), isNull);
    expect(namesDeploymentProfileForNetwork('test'), isNull);
    expect(kNamesDeploymentProfilesByNetwork['main'], isNull);
    expect(kNamesDeploymentProfilesByNetwork['test'], isNull);
  });

  test('only regtest maps to the local qualification profile', () {
    final profile = namesDeploymentProfileForNetwork('regtest');
    expect(profile, same(kLocalRegtestNamesDeploymentProfile));
    expect(profile!.isProduction, isFalse);
  });

  test('unknown network names normalize to mainnet and stay unconfigured', () {
    expect(namesDeploymentProfileForNetwork(''), isNull);
    expect(namesDeploymentProfileForNetwork('nonsense'), isNull);
  });

  test('the regtest profile matches the Rust live-smoke deployment', () {
    // Values must stay aligned with rust/src/wallet/tests/coppice.rs
    // (live_zaino_bootstrap_and_missing_resolution): activation 2/2,
    // epoch 8, TTL 15, refresh 16, lease 32, grace 3, reuse 4, record 1024,
    // bond 1, retention 128, domain coppice-runtime-regtest-v1.
    const profile = kLocalRegtestNamesDeploymentProfile;
    expect(profile.runtimeActivationHeight, 2);
    expect(profile.namesActivationHeight, 2);
    expect(profile.epochSize, 8);
    expect(profile.commitTtlBlocks, 15);
    expect(profile.refreshDeadlineBlocks, 16);
    expect(profile.leaseDurationBlocks, 32);
    expect(profile.gracePeriodBlocks, 3);
    expect(profile.reuseDelayBlocks, 4);
    expect(profile.maxRecordBytes, 1024);
    expect(profile.minimumBondZatoshis, 1);
    expect(profile.retentionBlocks, 128);
    expect(profile.networkDomain, 'coppice-runtime-regtest-v1');
    expect(profile.rendezvousIvkHex.length, 128);
    expect(profile.rendezvousReceiverHex.length, 86);
  });
}
