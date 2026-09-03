import '../../../core/config/network_config.dart';

const kNamesBondZatoshis = 100000000;

/// The chain identity and local replay policy for one Coppice/Names deployment.
///
/// Schedule fields are fixed identity-bearing values for this compiled
/// deployment, not user configuration. The one-ZEC bond is protocol-wide.
class NamesDeploymentProfile {
  const NamesDeploymentProfile({
    required this.label,
    required this.isProduction,
    required this.activationHeight,
    required this.epochBlocks,
    required this.windowBlocks,
    required this.commitMaturityBlocks,
    required this.commitTtlBlocks,
    required this.leaseBlocks,
    required this.cooldownBlocks,
    required this.retentionBlocks,
    required this.networkDomain,
    required this.rendezvousIvkHex,
    required this.rendezvousReceiverHex,
  });

  final String label;

  /// False for test-chain profiles, so the UI can label them as such and
  /// keep them visually distinct from any future production deployment.
  final bool isProduction;
  final int activationHeight;
  final int epochBlocks;
  final int windowBlocks;
  final int commitMaturityBlocks;
  final int commitTtlBlocks;
  final int leaseBlocks;
  final int cooldownBlocks;
  final int retentionBlocks;
  final String networkDomain;
  final String rendezvousIvkHex;
  final String rendezvousReceiverHex;
}

/// The local regtest qualification deployment — the same explicit values the
/// Rust wallet host configures against the local Zakura/Zaino regtest stack.
///
/// This is a test-chain configuration for development against the local
/// regtest node. It must never be presented or used as a production
/// deployment.
const kLocalRegtestNamesDeploymentProfile = NamesDeploymentProfile(
  label: 'Local regtest (qualification)',
  isProduction: false,
  activationHeight: 2,
  epochBlocks: 32,
  windowBlocks: 4,
  commitMaturityBlocks: 4,
  commitTtlBlocks: 24,
  leaseBlocks: 128,
  cooldownBlocks: 32,
  retentionBlocks: 128,
  networkDomain: 'coppice-runtime-regtest-v1',
  rendezvousIvkHex:
      '65deb2b3ee7ac69020543f40f21122cb6dc1f4201a329fcdf9d5e3bb2dfbbabe'
      '29d542352fe36c3c7b24c2989dc9d0000b9e04f444e05dc4538bde395c0e6008',
  rendezvousReceiverHex:
      '9ec59e4d447ba285086cc3456cadf62004a19b6a7989c726daaa9944a6cdbf25'
      'f7bfa51afa15b66da53881',
);

/// Explicit deployment profile per wallet network name ('main' | 'test' |
/// 'regtest').
///
/// Mainnet and testnet map to `null` on purpose: no public Coppice/Names
/// deployment exists yet, so the UI must show "not configured" there instead
/// of silently reusing the regtest identity. The Rust host enforces the same
/// boundary — `validated_core_parameters` refuses a configuration whose
/// network code does not match the wallet network.
const Map<String, NamesDeploymentProfile?> kNamesDeploymentProfilesByNetwork = {
  'main': null,
  'test': null,
  'regtest': kLocalRegtestNamesDeploymentProfile,
};

/// The deployment profile for [networkName], or `null` when that network has
/// no explicit Names deployment.
NamesDeploymentProfile? namesDeploymentProfileForNetwork(String networkName) {
  return kNamesDeploymentProfilesByNetwork[normalizeZcashNetworkName(
    networkName,
  )];
}
