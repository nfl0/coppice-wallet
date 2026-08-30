import '../../../core/config/network_config.dart';

/// An explicit Coppice/Names deployment configuration, mirroring the
/// parameters `NamesWalletConfig::from_api` validates on the Rust side.
///
/// The wallet host deliberately embeds no deployment values ("no embedded
/// test authority", rust/src/wallet/coppice.rs): parameters are supplied per
/// deployment, persisted immutably into the Names sidecar on configure, and
/// rejected when their network code does not match the wallet network. This
/// model keeps the same contract on the Dart side — a profile exists only
/// where a deployment was actually spelled out, and nothing below may fall
/// back to another network's identity.
class NamesDeploymentProfile {
  const NamesDeploymentProfile({
    required this.label,
    required this.isProduction,
    required this.runtimeActivationHeight,
    required this.namesActivationHeight,
    required this.epochSize,
    required this.commitTtlBlocks,
    required this.refreshDeadlineBlocks,
    required this.leaseDurationBlocks,
    required this.gracePeriodBlocks,
    required this.reuseDelayBlocks,
    required this.maxRecordBytes,
    required this.minimumBondZatoshis,
    required this.retentionBlocks,
    required this.networkDomain,
    required this.rendezvousIvkHex,
    required this.rendezvousReceiverHex,
  });

  final String label;

  /// False for test-chain profiles, so the UI can label them as such and
  /// keep them visually distinct from any future production deployment.
  final bool isProduction;
  final int runtimeActivationHeight;
  final int namesActivationHeight;
  final int epochSize;
  final int commitTtlBlocks;
  final int refreshDeadlineBlocks;
  final int leaseDurationBlocks;
  final int gracePeriodBlocks;
  final int reuseDelayBlocks;
  final int maxRecordBytes;
  final int minimumBondZatoshis;
  final int retentionBlocks;
  final String networkDomain;
  final String rendezvousIvkHex;
  final String rendezvousReceiverHex;
}

/// The local regtest qualification deployment — the same explicit values the
/// Rust wallet host configures against the local Zakura/Zaino regtest stack
/// (rust/src/wallet/tests/coppice.rs, `live_zaino_bootstrap_and_missing_resolution`).
///
/// This is a test-chain configuration for development against the local
/// regtest node. It must never be presented or used as a production
/// deployment.
const kLocalRegtestNamesDeploymentProfile = NamesDeploymentProfile(
  label: 'Local regtest (qualification)',
  isProduction: false,
  runtimeActivationHeight: 2,
  namesActivationHeight: 2,
  epochSize: 8,
  commitTtlBlocks: 15,
  refreshDeadlineBlocks: 16,
  leaseDurationBlocks: 32,
  gracePeriodBlocks: 3,
  reuseDelayBlocks: 4,
  maxRecordBytes: 1024,
  minimumBondZatoshis: 1,
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
  return kNamesDeploymentProfilesByNetwork[
    normalizeZcashNetworkName(networkName)
  ];
}
