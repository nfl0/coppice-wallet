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

  test(
    'the regtest profile contains only deployment identity and replay policy',
    () {
      const profile = kLocalRegtestNamesDeploymentProfile;
      expect(profile.activationHeight, 2);
      expect(profile.retentionBlocks, 128);
      expect(profile.networkDomain, 'coppice-runtime-regtest-v1');
      expect(profile.rendezvousIvkHex.length, 128);
      expect(profile.rendezvousReceiverHex.length, 86);
    },
  );

  test('replacement protocol constants match the Rust deployment rules', () {
    expect(kNamesEpochBlocks, 1152);
    expect(kNamesWindowBlocks, 24);
    expect(kNamesCommitMaturityBlocks, 24);
    expect(kNamesCommitTtlBlocks, 192);
    expect(kNamesLeaseBlocks, 250000);
    expect(kNamesCooldownBlocks, 1152);
    expect(kNamesBondZatoshis, 100000000);
  });
}
