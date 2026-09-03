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
    'the regtest profile contains its accelerated identity and replay policy',
    () {
      const profile = kLocalRegtestNamesDeploymentProfile;
      expect(profile.activationHeight, 2);
      expect(profile.epochBlocks, 32);
      expect(profile.windowBlocks, 4);
      expect(profile.commitMaturityBlocks, 4);
      expect(profile.commitTtlBlocks, 24);
      expect(profile.leaseBlocks, 128);
      expect(profile.cooldownBlocks, 32);
      expect(profile.retentionBlocks, 128);
      expect(profile.networkDomain, 'coppice-runtime-regtest-v1');
      expect(profile.rendezvousIvkHex.length, 128);
      expect(profile.rendezvousReceiverHex.length, 86);
    },
  );

  test('replacement protocol keeps the exact one-ZEC bond', () {
    expect(kNamesBondZatoshis, 100000000);
  });
}
