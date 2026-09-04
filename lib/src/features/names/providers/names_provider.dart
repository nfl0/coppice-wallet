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

  /// The running action. Currently only `configure` is user initiated.
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
    this.draftName,
    this.draftPaymentAddress,
    this.draftPhase,
    this.commitBlocksUntil,
    this.commitWindowOpen = false,
    this.error,
  });

  final bool inFlight;
  final rust_names.ApiNamesBondStatus? bondStatus;
  final String? draftName;
  final String? draftPaymentAddress;
  final String? draftPhase;
  final BigInt? commitBlocksUntil;
  final bool commitWindowOpen;
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

  /// Clears the UI's completed workflow selection while keeping the latest
  /// bond inventory. A wallet can register another name after a prior one
  /// reaches the active state.
  void resetDraft() {
    state = NamesRegistrationState(bondStatus: state.bondStatus);
  }

  void reportError(String message) {
    state = NamesRegistrationState(
      bondStatus: state.bondStatus,
      draftName: state.draftName,
      draftPaymentAddress: state.draftPaymentAddress,
      draftPhase: state.draftPhase,
      commitBlocksUntil: state.commitBlocksUntil,
      commitWindowOpen: state.commitWindowOpen,
      error: message,
    );
  }

  void resumeDraft({
    required String name,
    required String paymentAddress,
    required String phase,
  }) {
    state = NamesRegistrationState(
      bondStatus: state.bondStatus,
      draftName: name,
      draftPaymentAddress: paymentAddress,
      draftPhase: phase,
    );
  }

  Future<rust_names.ApiNamesBondStatus?> refreshBondStatus() async {
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) return null;
    try {
      final endpoint = ref.read(rpcEndpointProvider);
      final status = rust_names.getNamesBondStatus(
        dbPath: await getWalletDbPath(),
        network: endpoint.networkName,
        accountUuid: accountUuid,
      );
      state = NamesRegistrationState(
        bondStatus: status,
        draftName: state.draftName,
        draftPaymentAddress: state.draftPaymentAddress,
        draftPhase: state.draftPhase,
      );
      return status;
    } catch (error) {
      log('Names: bond inventory failed: $error');
      // A transient sync/SQLite error must not erase a durable registration
      // draft. Keep the phase so the next refresh can expose COMMIT once the
      // exact bond has been reserved.
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        draftName: state.draftName,
        draftPaymentAddress: state.draftPaymentAddress,
        draftPhase: state.draftPhase,
        error: _friendlyRegistrationError(error),
      );
      return null;
    }
  }

  Future<String?> prepareDraft({
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
    final validationError = zecNameLabelValidationError(name);
    if (validationError != null) {
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        error: validationError,
      );
      return null;
    }
    state = NamesRegistrationState(
      inFlight: true,
      bondStatus: state.bondStatus,
      draftName: name.trim().toLowerCase(),
      draftPaymentAddress: paymentAddress.trim(),
    );
    final endpoint = ref.read(rpcEndpointProvider);
    try {
      final dbPath = await getWalletDbPath();
      final namesStatus = rust_names.getNamesStatus(
        dbPath: dbPath,
        network: endpoint.networkName,
      );
      if (namesStatus.state == 'needs_bootstrap') {
        // The replacement protocol has no separate global-bootstrap user
        // action. Authenticate the exact name needed by this registration;
        // the resulting checkpoint is then reused by lifecycle construction.
        await rust_names.resolveName(
          dbPath: dbPath,
          lightwalletdUrl: endpoint.normalizedLightwalletdUrl,
          network: endpoint.networkName,
          name: '${name.trim().toLowerCase()}.zec',
        );
        ref.invalidate(namesStatusProvider);
      }
      final mnemonicBytes = await accountNotifier.getMnemonicBytesForAccount(
        accountUuid,
      );
      if (mnemonicBytes == null || mnemonicBytes.isEmpty) {
        state = const NamesRegistrationState(
          error: 'The selected account credential is unavailable.',
        );
        return null;
      }
      late final Future<rust_names.ApiNamesRegistrationDraft> draftFuture;
      try {
        draftFuture = rust_names.prepareNamesRegistrationDraft(
          dbPath: dbPath,
          network: endpoint.networkName,
          accountUuid: accountUuid,
          name: name,
          paymentAddress: paymentAddress,
          mnemonicBytes: mnemonicBytes,
        );
      } finally {
        mnemonicBytes.fillRange(0, mnemonicBytes.length, 0);
      }
      final draft = await draftFuture;
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        draftName: name.trim().toLowerCase(),
        draftPaymentAddress: paymentAddress.trim(),
        draftPhase: draft.phase,
      );
      return draft.phase;
    } catch (error) {
      log('Names: registration draft failed: $error');
      state = NamesRegistrationState(error: _friendlyRegistrationError(error));
      return null;
    }
  }

  Future<void> refreshDraftPhase() async {
    final draftName = state.draftName;
    final accountUuid = ref.read(accountProvider).value?.activeAccountUuid;
    if (draftName == null || accountUuid == null) return;
    try {
      final endpoint = ref.read(rpcEndpointProvider);
      final names = rust_names.getManagedNames(
        dbPath: await getWalletDbPath(),
        network: endpoint.networkName,
        accountUuid: accountUuid,
      );
      rust_names.ApiManagedName? draft;
      for (final item in names) {
        if (item.name == draftName) {
          draft = item;
          break;
        }
      }
      if (draft == null) return;
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        draftName: draftName,
        draftPaymentAddress: state.draftPaymentAddress,
        draftPhase: draft.workflowPhase,
        commitBlocksUntil: draft.commitBlocksUntil,
        commitWindowOpen: draft.commitWindowOpen,
      );
    } catch (error) {
      log('Names: registration draft refresh failed: $error');
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
    final validationError = zecNameLabelValidationError(name);
    if (validationError != null) {
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        error: validationError,
      );
      return null;
    }
    state = NamesRegistrationState(
      inFlight: true,
      bondStatus: state.bondStatus,
      draftName: state.draftName,
      draftPaymentAddress: state.draftPaymentAddress,
      draftPhase: state.draftPhase,
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
        proposalFuture = rust_names.beginNamesRegistration(
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
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        draftName: state.draftName,
        draftPaymentAddress: state.draftPaymentAddress,
        draftPhase: state.draftPhase,
      );
      return SendReviewArgs(
        proposalId: proposal.proposalId,
        sendFlowId: sendFlowId,
        proposalAccountUuid: accountUuid,
        address: 'Coppice Names COMMIT',
        addressType: 'unified',
        amountZatoshi: BigInt.from(kNamesBondZatoshis),
        feeZatoshi: proposal.feeZatoshi,
        needsSaplingParams: false,
        memo: 'Register ${name.trim().toLowerCase()}',
        cancelLocation: '/names',
        completionLocation: '/names',
      );
    } catch (error) {
      log('Names: registration proposal failed: $error');
      state = NamesRegistrationState(
        bondStatus: state.bondStatus,
        draftName: state.draftName,
        draftPaymentAddress: state.draftPaymentAddress,
        draftPhase: state.draftPhase,
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

String _friendlyRecoveryError(Object error) {
  final text = error.toString().replaceFirst(RegExp(r'^Exception:\s*'), '');
  final lower = text.toLowerCase();
  if (lower.contains('does not own')) {
    return 'This wallet does not own the accepted bond for that name.';
  }
  if (lower.contains('not currently owned') ||
      lower.contains('no authenticated names state')) {
    return 'That name has no recoverable owner state.';
  }
  return text;
}

/// Coppice/Names wallet status for the active network, or `null` while no
/// account is active (the Rust host requires the wallet DB to exist).
final namesStatusProvider =
    AsyncNotifierProvider<
      NamesStatusNotifier,
      rust_names.ApiNamesWalletStatus?
    >(NamesStatusNotifier.new);

final managedNamesProvider =
    AsyncNotifierProvider<
      ManagedNamesNotifier,
      List<rust_names.ApiManagedName>
    >(ManagedNamesNotifier.new);

class ManagedNamesNotifier
    extends AsyncNotifier<List<rust_names.ApiManagedName>> {
  String? _lastRevealError;
  String? _lastManagementError;

  String? get lastRevealError => _lastRevealError;
  String? get lastManagementError => _lastManagementError;

  @override
  Future<List<rust_names.ApiManagedName>> build() async {
    final accountUuid = ref.watch(accountProvider).value?.activeAccountUuid;
    if (accountUuid == null) return const [];
    final endpoint = ref.watch(rpcEndpointProvider);
    return rust_names.getManagedNames(
      dbPath: await getWalletDbPath(),
      network: endpoint.networkName,
      accountUuid: accountUuid,
    );
  }

  Future<void> refresh() async {
    ref.invalidateSelf();
    await future;
  }

  /// Explicitly recovers one user-supplied name. Ordinary lookup never calls
  /// this method and therefore never derives ownership or mutates `Your names`.
  Future<String?> recover(String name) async {
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) return 'Unlock your wallet first.';
    final accountNotifier = ref.read(accountProvider.notifier);
    if (accountNotifier.isHardwareAccount(accountUuid)) {
      return 'Names recovery currently requires a software account.';
    }
    final mnemonicBytes = await accountNotifier.getMnemonicBytesForAccount(
      accountUuid,
    );
    if (mnemonicBytes == null || mnemonicBytes.isEmpty) {
      return 'The selected account credential is unavailable.';
    }
    final endpoint = ref.read(rpcEndpointProvider);
    try {
      late final Future<void> recoveryFuture;
      try {
        recoveryFuture = rust_names.recoverNamesRegistration(
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
      await recoveryFuture;
      if (ref.mounted) {
        ref.invalidateSelf();
        ref.invalidate(namesStatusProvider);
      }
      return null;
    } catch (error) {
      log('Names: recovery failed: $error');
      return _friendlyRecoveryError(error);
    }
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
        revealFuture = rust_names.revealNamesRegistration(
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

  /// Builds the reviewed Names REVEAL capability without broadcasting it.
  /// The returned args enter the same send review/status flow as ordinary
  /// wallet sends; no background scheduler is involved.
  Future<SendReviewArgs?> beginReveal(String name) async {
    _lastRevealError = null;
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) {
      _lastRevealError = 'Unlock your wallet first.';
      return null;
    }
    final accountNotifier = ref.read(accountProvider.notifier);
    if (accountNotifier.isHardwareAccount(accountUuid)) {
      _lastRevealError = 'Names REVEAL currently requires a software account.';
      return null;
    }
    final mnemonicBytes = await accountNotifier.getMnemonicBytesForAccount(
      accountUuid,
    );
    if (mnemonicBytes == null || mnemonicBytes.isEmpty) {
      _lastRevealError = 'The selected account credential is unavailable.';
      return null;
    }
    final endpoint = ref.read(rpcEndpointProvider);
    final sendFlowId = newSendFlowId();
    try {
      late final Future<rust_names.ApiNamesRevealProposal> proposalFuture;
      try {
        proposalFuture = rust_names.beginNamesReveal(
          dbPath: await getWalletDbPath(),
          lightwalletdUrl: endpoint.normalizedLightwalletdUrl,
          network: endpoint.networkName,
          accountUuid: accountUuid,
          sendFlowId: sendFlowId,
          name: name,
          mnemonicBytes: mnemonicBytes,
        );
      } finally {
        mnemonicBytes.fillRange(0, mnemonicBytes.length, 0);
      }
      final proposal = await proposalFuture;
      return SendReviewArgs(
        proposalId: proposal.proposalId,
        sendFlowId: sendFlowId,
        proposalAccountUuid: accountUuid,
        address: 'Coppice Names REVEAL',
        addressType: 'unified',
        amountZatoshi: BigInt.from(kNamesBondZatoshis),
        feeZatoshi: proposal.feeZatoshi,
        needsSaplingParams: false,
        memo: 'Reveal ${name.trim().toLowerCase()}',
        cancelLocation: '/names',
        completionLocation: '/names',
      );
    } catch (error) {
      log('Names: REVEAL proposal failed: $error');
      _lastRevealError = _friendlyRegistrationError(error);
      return null;
    }
  }

  Future<String?> discardUncompletedRegistration(String name) async {
    final accountUuid = ref.read(accountProvider).value?.activeAccountUuid;
    if (accountUuid == null) return 'Unlock your wallet first.';
    try {
      final endpoint = ref.read(rpcEndpointProvider);
      rust_names.discardNamesRegistrationWorkflow(
        dbPath: await getWalletDbPath(),
        network: endpoint.networkName,
        accountUuid: accountUuid,
        name: name,
      );
      if (ref.mounted) ref.invalidateSelf();
      return null;
    } catch (error) {
      log('Names: discard registration failed: $error');
      return _friendlyRegistrationError(error);
    }
  }

  /// Builds a reviewed UPDATE, RENEW, or RELEASE capability without
  /// broadcasting it. The shared send review/status flow owns the proposal
  /// after this returns.
  Future<SendReviewArgs?> beginManagement(
    String name,
    String action, {
    String? paymentAddress,
  }) async {
    _lastManagementError = null;
    final account = ref.read(accountProvider).value;
    final accountUuid = account?.activeAccountUuid;
    if (accountUuid == null) {
      _lastManagementError = 'Unlock your wallet first.';
      return null;
    }
    final accountNotifier = ref.read(accountProvider.notifier);
    if (accountNotifier.isHardwareAccount(accountUuid)) {
      _lastManagementError =
          'Names management currently requires a software account.';
      return null;
    }
    final mnemonicBytes = await accountNotifier.getMnemonicBytesForAccount(
      accountUuid,
    );
    if (mnemonicBytes == null || mnemonicBytes.isEmpty) {
      _lastManagementError = 'The selected account credential is unavailable.';
      return null;
    }
    final endpoint = ref.read(rpcEndpointProvider);
    final sendFlowId = newSendFlowId();
    try {
      late final Future<rust_names.ApiNamesRevealProposal> proposalFuture;
      try {
        proposalFuture = rust_names.beginNamesManagement(
          dbPath: await getWalletDbPath(),
          lightwalletdUrl: endpoint.normalizedLightwalletdUrl,
          network: endpoint.networkName,
          accountUuid: accountUuid,
          sendFlowId: sendFlowId,
          name: name,
          action: action,
          paymentAddress: paymentAddress,
          mnemonicBytes: mnemonicBytes,
        );
      } finally {
        mnemonicBytes.fillRange(0, mnemonicBytes.length, 0);
      }
      final proposal = await proposalFuture;
      final canonicalName = name.trim().toLowerCase();
      final operation = action.trim().toUpperCase();
      return SendReviewArgs(
        proposalId: proposal.proposalId,
        sendFlowId: sendFlowId,
        proposalAccountUuid: accountUuid,
        address: 'Coppice Names $operation',
        addressType: 'unified',
        amountZatoshi: BigInt.from(kNamesBondZatoshis),
        feeZatoshi: proposal.feeZatoshi,
        needsSaplingParams: false,
        memo: '${_managementVerb(action)} $canonicalName',
        cancelLocation: '/names',
        completionLocation: '/names',
      );
    } catch (error) {
      log('Names: $action proposal failed: $error');
      _lastManagementError = _friendlyRegistrationError(error);
      return null;
    }
  }

  /// Direct execution retained for the live protocol qualification harness.
  /// Interactive wallet UI must use [beginManagement] so the user reviews and
  /// authorizes the transaction through the shared send flow.
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
        operationFuture = rust_names.manageName(
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
      log('Names qualification: $action failed: $error');
      return _friendlyRegistrationError(error);
    }
  }
}

String _managementVerb(String action) => switch (action) {
  'update' => 'Update',
  'renew' => 'Renew',
  'release' => 'Release',
  _ => 'Manage',
};

class NamesStatusNotifier
    extends AsyncNotifier<rust_names.ApiNamesWalletStatus?> {
  @override
  Future<rust_names.ApiNamesWalletStatus?> build() async {
    final accountUuid = ref.watch(accountProvider).value?.activeAccountUuid;
    if (accountUuid == null) return null;
    final endpoint = ref.watch(rpcEndpointProvider);
    final dbPath = await getWalletDbPath();
    return rust_names.getNamesStatus(
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
      final status = rust_names.configureNames(
        dbPath: dbPath,
        network: endpoint.networkName,
        retentionBlocks: BigInt.from(profile.retentionBlocks),
      );
      if (!ref.mounted) return;
      action.succeed();
      state = AsyncData(status);
    } catch (error) {
      log('Names: configure failed: $error');
      if (!ref.mounted) return;
      action.fail(friendlyNamesActionError(error));
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

String friendlyNamesActionError(Object error) {
  final text = error.toString().toLowerCase();
  if (text.contains('different wallet network')) {
    return 'The stored Names configuration belongs to a different wallet '
        'network. Reconfigure this wallet explicitly.';
  }
  if (text.contains('already') || text.contains('exists')) {
    return 'Names is already configured for this wallet.';
  }
  return "Couldn't enable Coppice Names. Try again in a moment.";
}
