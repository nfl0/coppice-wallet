import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../../main.dart' show log;
import '../../../core/storage/wallet_paths.dart';
import '../../../providers/account_provider.dart';
import '../../../providers/rpc_endpoint_provider.dart';
import '../../../rust/api/names.dart' as rust_names;
import '../../send/services/send_flow.dart';
import '../models/names_deployment.dart';
import '../services/zec_name_resolution.dart';

/// Deployment profile for the active wallet network, or `null` when that
/// network has no explicit Names deployment (mainnet and testnet today —
/// see [kNamesDeploymentProfilesByNetwork] for the explicitness contract).
final namesDeploymentProfileProvider = Provider<NamesDeploymentProfile?>((ref) {
  final endpoint = ref.watch(rpcEndpointProvider);
  return namesDeploymentProfileForNetwork(endpoint.networkName);
});

/// The status-card action currently running plus its last failure, so the
/// status value and action progress compose without clobbering each other.
final namesActionProvider =
    NotifierProvider<NamesActionNotifier, NamesActionState>(
      NamesActionNotifier.new,
    );

class NamesActionState {
  const NamesActionState({this.inFlight, this.error});

  /// The running action: 'configure' | 'bootstrap'.
  final String? inFlight;
  final String? error;
}

class NamesActionNotifier extends Notifier<NamesActionState> {
  @override
  NamesActionState build() => const NamesActionState();

  void begin(String action) => state = NamesActionState(inFlight: action);

  void fail(String message) => state = NamesActionState(error: message);

  void succeed() => state = const NamesActionState();
}

class NamesRegistrationState {
  const NamesRegistrationState({
    this.inFlight = false,
    this.bondStatus,
    this.error,
  });

  final bool inFlight;
  final rust_names.ApiNamesBondStatus? bondStatus;
  final String? error;
}

/// Registration orchestration stops at the ordinary wallet review screen.
/// Names chooses and reserves only its exact bond; wallet fee selection,
/// credentials, proving and broadcast remain owned by the established send
/// pipeline.
final namesRegistrationProvider =
    NotifierProvider<NamesRegistrationNotifier, NamesRegistrationState>(
      NamesRegistrationNotifier.new,
    );

class NamesRegistrationNotifier extends Notifier<NamesRegistrationState> {
  @override
  NamesRegistrationState build() => const NamesRegistrationState();

  Future<rust_names.ApiNamesBondStatus?> refreshBondStatus() async {
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) return null;
    try {
      final endpoint = ref.read(rpcEndpointProvider);
      final status = rust_names.getNamesV1BondStatus(
        dbPath: await getWalletDbPath(),
        network: endpoint.networkName,
        accountUuid: accountUuid,
      );
      state = NamesRegistrationState(bondStatus: status);
      return status;
    } catch (error) {
      log('Names: bond inventory failed: $error');
      state = NamesRegistrationState(error: _friendlyRegistrationError(error));
      return null;
    }
  }

  Future<SendReviewArgs?> begin({
    required String name,
    required String paymentAddress,
  }) async {
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) {
      state = const NamesRegistrationState(error: 'Unlock your wallet first.');
      return null;
    }
    final accountNotifier = ref.read(accountProvider.notifier);
    if (accountNotifier.isHardwareAccount(accountUuid)) {
      state = const NamesRegistrationState(
        error: 'Names registration currently requires a software account.',
      );
      return null;
    }
    state = NamesRegistrationState(
      inFlight: true,
      bondStatus: state.bondStatus,
    );
    final mnemonicBytes = await accountNotifier.getMnemonicBytesForAccount(
      accountUuid,
    );
    if (mnemonicBytes == null || mnemonicBytes.isEmpty) {
      state = const NamesRegistrationState(
        error: 'The selected account credential is unavailable.',
      );
      return null;
    }
    final endpoint = ref.read(rpcEndpointProvider);
    final sendFlowId = newSendFlowId();
    try {
      late final Future<rust_names.ApiNamesCommitProposal> proposalFuture;
      try {
        proposalFuture = rust_names.beginNamesV1Registration(
          dbPath: await getWalletDbPath(),
          network: endpoint.networkName,
          accountUuid: accountUuid,
          sendFlowId: sendFlowId,
          name: name,
          paymentAddress: paymentAddress,
          mnemonicBytes: mnemonicBytes,
        );
      } finally {
        mnemonicBytes.fillRange(0, mnemonicBytes.length, 0);
      }
      final proposal = await proposalFuture;
      state = NamesRegistrationState(bondStatus: state.bondStatus);
      return SendReviewArgs(
        proposalId: proposal.proposalId,
        sendFlowId: sendFlowId,
        proposalAccountUuid: accountUuid,
        address: 'Coppice Names COMMIT',
        addressType: 'unified',
        amountZatoshi: BigInt.one,
        feeZatoshi: proposal.feeZatoshi,
        needsSaplingParams: false,
        memo: 'Register ${name.trim().toLowerCase()}',
      );
    } catch (error) {
      log('Names: registration proposal failed: $error');
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        error: _friendlyRegistrationError(error),
      );
      return null;
    }
  }
}

String _friendlyRegistrationError(Object error) {
  final text = error.toString();
  final lower = text.toLowerCase();
  if (lower.contains('exact') && lower.contains('one-zec')) {
    return 'Prepare an exact 1 ZEC Ironwood note before registering.';
  }
  if (lower.contains('insufficient')) return 'The wallet has insufficient ZEC.';
  return text.replaceFirst(RegExp(r'^Exception:\s*'), '');
}

/// Coppice/Names wallet status for the active network, or `null` while no
/// account is active (the Rust host requires the wallet DB to exist).
final namesStatusProvider =
    AsyncNotifierProvider<
      NamesStatusNotifier,
      rust_names.ApiNamesWalletStatus?
    >(NamesStatusNotifier.new);

final managedNamesProvider =
    AsyncNotifierProvider<ManagedNamesNotifier, List<rust_names.ApiManagedName>>(
      ManagedNamesNotifier.new,
    );

class ManagedNamesNotifier
    extends AsyncNotifier<List<rust_names.ApiManagedName>> {
  @override
  Future<List<rust_names.ApiManagedName>> build() async {
    final accountUuid = ref.watch(accountProvider).value?.activeAccountUuid;
    if (accountUuid == null) return const [];
    final endpoint = ref.watch(rpcEndpointProvider);
    return rust_names.getManagedNamesV1(
      dbPath: await getWalletDbPath(),
      network: endpoint.networkName,
      accountUuid: accountUuid,
    );
  }

  Future<void> refresh() async {
    ref.invalidateSelf();
    await future;
  }

  Future<String?> reveal(String name) async {
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) return 'Unlock your wallet first.';
    final accountNotifier = ref.read(accountProvider.notifier);
    if (accountNotifier.isHardwareAccount(accountUuid)) {
      return 'Names REVEAL currently requires a software account.';
    }
    final mnemonicBytes = await accountNotifier.getMnemonicBytesForAccount(
      accountUuid,
    );
    if (mnemonicBytes == null || mnemonicBytes.isEmpty) {
      return 'The selected account credential is unavailable.';
    }
    final endpoint = ref.read(rpcEndpointProvider);
    try {
      late final Future<List<int>> revealFuture;
      try {
        revealFuture = rust_names.revealNamesV1Registration(
          dbPath: await getWalletDbPath(),
          lightwalletdUrl: endpoint.normalizedLightwalletdUrl,
          network: endpoint.networkName,
          accountUuid: accountUuid,
          name: name,
          mnemonicBytes: mnemonicBytes,
        );
      } finally {
        mnemonicBytes.fillRange(0, mnemonicBytes.length, 0);
      }
      await revealFuture;
      if (ref.mounted) {
        ref.invalidateSelf();
        ref.invalidate(namesStatusProvider);
      }
      return null;
    } catch (error) {
      log('Names: REVEAL failed: $error');
      return _friendlyRegistrationError(error);
    }
  }

  Future<String?> manage(
    String name,
    String action, {
    String? paymentAddress,
  }) async {
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) return 'Unlock your wallet first.';
    final accountNotifier = ref.read(accountProvider.notifier);
    if (accountNotifier.isHardwareAccount(accountUuid)) {
      return 'Names management currently requires a software account.';
    }
    final mnemonicBytes = await accountNotifier.getMnemonicBytesForAccount(
      accountUuid,
    );
    if (mnemonicBytes == null || mnemonicBytes.isEmpty) {
      return 'The selected account credential is unavailable.';
    }
    final endpoint = ref.read(rpcEndpointProvider);
    try {
      late final Future<List<int>> operationFuture;
      try {
        operationFuture = rust_names.manageNameV1(
          dbPath: await getWalletDbPath(),
          lightwalletdUrl: endpoint.normalizedLightwalletdUrl,
          network: endpoint.networkName,
          accountUuid: accountUuid,
          name: name,
          action: action,
          paymentAddress: paymentAddress,
          mnemonicBytes: mnemonicBytes,
        );
      } finally {
        mnemonicBytes.fillRange(0, mnemonicBytes.length, 0);
      }
      await operationFuture;
      if (ref.mounted) ref.invalidateSelf();
      return null;
    } catch (error) {
      log('Names: $action failed: $error');
      return _friendlyRegistrationError(error);
    }
  }
}

class NamesStatusNotifier
    extends AsyncNotifier<rust_names.ApiNamesWalletStatus?> {
  @override
  Future<rust_names.ApiNamesWalletStatus?> build() async {
    final accountUuid = ref.watch(accountProvider).value?.activeAccountUuid;
    if (accountUuid == null) return null;
    final endpoint = ref.watch(rpcEndpointProvider);
    final dbPath = await getWalletDbPath();
    return rust_names.getNamesV1Status(
      dbPath: dbPath,
      network: endpoint.networkName,
    );
  }

  Future<void> refresh() async {
    ref.invalidateSelf();
    await future;
  }

  /// Writes the network's explicit deployment profile into the wallet's
  /// Names sidecar. Refuses when the network has no profile — deployment
  /// values must never be invented from another network's identity.
  Future<void> configureWithDeploymentProfile() async {
    final action = ref.read(namesActionProvider.notifier);
    final profile = ref.read(namesDeploymentProfileProvider);
    if (profile == null) {
      action.fail(
        'No Coppice/Names deployment is configured for this network. '
        'Deployment parameters must be supplied explicitly.',
      );
      return;
    }
    action.begin('configure');
    try {
      final endpoint = ref.read(rpcEndpointProvider);
      final dbPath = await getWalletDbPath();
      final status = rust_names.configureNamesV1(
        dbPath: dbPath,
        network: endpoint.networkName,
        runtimeActivationHeight: BigInt.from(profile.runtimeActivationHeight),
        namesActivationHeight: BigInt.from(profile.namesActivationHeight),
        epochSize: BigInt.from(profile.epochSize),
        commitTtlBlocks: BigInt.from(profile.commitTtlBlocks),
        refreshDeadlineBlocks: BigInt.from(profile.refreshDeadlineBlocks),
        leaseDurationBlocks: BigInt.from(profile.leaseDurationBlocks),
        gracePeriodBlocks: BigInt.from(profile.gracePeriodBlocks),
        reuseDelayBlocks: BigInt.from(profile.reuseDelayBlocks),
        maxRecordBytes: BigInt.from(profile.maxRecordBytes),
        minimumBondZatoshis: BigInt.from(profile.minimumBondZatoshis),
        retentionBlocks: BigInt.from(profile.retentionBlocks),
        networkDomain: profile.networkDomain,
        rendezvousIvkHex: profile.rendezvousIvkHex,
        rendezvousReceiverHex: profile.rendezvousReceiverHex,
      );
      if (!ref.mounted) return;
      action.succeed();
      state = AsyncData(status);
    } catch (error) {
      log('Names: configure failed: $error');
      if (!ref.mounted) return;
      action.fail(friendlyNamesActionError(error, 'configure'));
    }
  }

  /// Streams the chain through the Names host so the sidecar reaches
  /// `ready`, using the wallet's active lightwalletd endpoint.
  Future<void> bootstrapFromActiveEndpoint() async {
    final action = ref.read(namesActionProvider.notifier);
    action.begin('bootstrap');
    try {
      final endpoint = ref.read(rpcEndpointProvider);
      final dbPath = await getWalletDbPath();
      final status = await rust_names.bootstrapNamesV1(
        dbPath: dbPath,
        lightwalletdUrl: endpoint.normalizedLightwalletdUrl,
        network: endpoint.networkName,
      );
      if (!ref.mounted) return;
      action.succeed();
      state = AsyncData(status);
    } catch (error) {
      log('Names: bootstrap failed: $error');
      if (!ref.mounted) return;
      action.fail(friendlyNamesActionError(error, 'bootstrap'));
    }
  }
}

/// State of the on-screen name lookup.
class NameLookupState {
  const NameLookupState({this.query, this.resolution, this.error});

  final String? query;
  final ZecNameResolution? resolution;
  final String? error;

  bool get hasResult => resolution != null;
}

/// Exact-name lookup for the Names screen. Resolves are seq-guarded so a
/// slow response for an earlier query can never overwrite a newer one.
final nameLookupProvider =
    NotifierProvider<NameLookupNotifier, NameLookupState>(
      NameLookupNotifier.new,
    );

class NameLookupNotifier extends Notifier<NameLookupState> {
  var _seq = 0;

  @override
  NameLookupState build() => const NameLookupState();

  Future<void> resolve(String input) async {
    final seq = ++_seq;
    final name = input.trim().toLowerCase();
    state = NameLookupState(query: name);
    final endpoint = ref.read(rpcEndpointProvider);
    try {
      final dbPath = await getWalletDbPath();
      final resolution = await ref.read(zecNameResolverProvider)(
        name,
        dbPath: dbPath,
        lightwalletdUrl: endpoint.normalizedLightwalletdUrl,
        network: endpoint.networkName,
      );
      if (!ref.mounted || seq != _seq) return;
      state = NameLookupState(query: resolution.name, resolution: resolution);
    } on ZecNameResolutionException catch (error) {
      if (!ref.mounted || seq != _seq) return;
      state = NameLookupState(query: name, error: error.message);
    } catch (error) {
      log('Names: lookup failed: $error');
      if (!ref.mounted || seq != _seq) return;
      state = NameLookupState(
        query: name,
        error: friendlyZecNameResolutionError(error),
      );
    }
  }

  void clear() {
    _seq++;
    state = const NameLookupState();
  }
}

String friendlyNamesActionError(Object error, String action) {
  final text = error.toString().toLowerCase();
  if (text.contains('different wallet network')) {
    return 'The stored Names configuration belongs to a different wallet '
        'network. Reconfigure this wallet explicitly.';
  }
  if (text.contains('already') || text.contains('exists')) {
    return 'Names is already configured for this wallet.';
  }
  return action == 'bootstrap'
      ? "Couldn't bootstrap Coppice Names. Check your connection and try again."
      : "Couldn't enable Coppice Names. Try again in a moment.";
}
