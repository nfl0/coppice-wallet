import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';

import '../../../core/layout/app_desktop_shell.dart';
import '../../../core/layout/app_main_sidebar.dart';
import '../../../core/theme/app_theme.dart';
import '../../../core/widgets/app_button.dart';
import '../../../core/widgets/app_copy_feedback.dart';
import '../../../core/widgets/app_icon.dart';
import '../../../core/widgets/app_icon_hover_button.dart';
import '../../../core/widgets/app_text_field.dart';
import '../../../rust/api/names.dart' as rust_names;
import '../../../providers/account_provider.dart';
import '../../../providers/sync_provider.dart';
import '../../send/models/send_prefill_args.dart';
import '../../send/services/send_flow.dart' show discardSendProposal;
import '../models/names_deployment.dart';
import '../providers/names_provider.dart';
import '../services/zec_name_resolution.dart';

/// Desktop Coppice/Names destination: the shared pane inside the app shell.
class NamesScreen extends StatelessWidget {
  const NamesScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return const AppDesktopShell(
      sidebar: AppMainSidebar(),
      pane: AppDesktopPane(
        padding: EdgeInsets.zero,
        child: NamesView(showDesktopChrome: true),
      ),
    );
  }
}

/// Shared Coppice/Names pane content. Platform screens own the surrounding
/// navigation chrome (desktop pane toolbar / mobile shell), like voting's
/// `VotingPollsView`.
class NamesView extends ConsumerStatefulWidget {
  const NamesView({required this.showDesktopChrome, super.key});

  final bool showDesktopChrome;

  @override
  ConsumerState<NamesView> createState() => _NamesViewState();
}

class _NamesViewState extends ConsumerState<NamesView> {
  final _nameController = TextEditingController();
  final _registrationNameController = TextEditingController();
  final _registrationAddressController = TextEditingController();
  String? _managedNameInFlight;
  String? _managedNameError;

  @override
  void initState() {
    super.initState();
    // Rebuild on keystrokes so the Resolve button tracks the field text.
    _nameController.addListener(_handleNameChanged);
    _registrationNameController.addListener(_handleNameChanged);
    _registrationAddressController.addListener(_handleNameChanged);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted || _registrationAddressController.text.isNotEmpty) return;
      _registrationAddressController.text =
          ref.read(accountProvider).value?.activeAddress ?? '';
      final registration = ref.read(namesRegistrationProvider);
      if (registration.draftName != null) {
        _registrationNameController.text = registration.draftName!;
      }
      if (registration.draftPaymentAddress != null) {
        _registrationAddressController.text = registration.draftPaymentAddress!;
      }
      unawaited(_refreshRegistrationState());
    });
  }

  Future<void> _refreshRegistrationState() async {
    final notifier = ref.read(namesRegistrationProvider.notifier);
    await notifier.refreshBondStatus();
    if (!mounted) return;
    await notifier.refreshDraftPhase();
  }

  @override
  void dispose() {
    _nameController.removeListener(_handleNameChanged);
    _nameController.dispose();
    _registrationNameController.dispose();
    _registrationAddressController.dispose();
    super.dispose();
  }

  void _handleNameChanged() {
    if (mounted) setState(() {});
  }

  Future<void> _refreshNamesAfterCompletedSync() async {
    await ref.read(namesStatusProvider.notifier).refresh();
    if (!mounted) return;
    await ref.read(managedNamesProvider.notifier).refresh();
    if (!mounted) return;
    await ref.read(namesRegistrationProvider.notifier).refreshDraftPhase();
  }

  void _resolveName() {
    final name = _nameController.text.trim();
    if (name.isEmpty) return;
    ref.read(nameLookupProvider.notifier).resolve(name);
  }

  void _sendToResolution(ZecNameResolution resolution) {
    final args = SendPrefillArgs(
      id: 'names-${resolution.name}',
      source: 'names',
      // Keep the name as the user's recipient intent. The send screen resolves
      // it itself and revalidates it before proposal creation.
      address: resolution.name,
      label: resolution.name,
    );
    context.go('/send', extra: args);
  }

  Future<void> _registerName() async {
    final notifier = ref.read(namesRegistrationProvider.notifier);
    var registration = ref.read(namesRegistrationProvider);
    final enteredName = _registrationNameController.text.trim().toLowerCase();
    if (registration.draftName == null ||
        (registration.draftPhase == 'active' &&
            registration.draftName != enteredName)) {
      if (registration.draftName != null) notifier.resetDraft();
      await notifier.prepareDraft(
        name: _registrationNameController.text,
        paymentAddress: _registrationAddressController.text,
      );
      if (!mounted) return;
      registration = ref.read(namesRegistrationProvider);
    } else {
      await notifier.refreshDraftPhase();
      if (!mounted) return;
      registration = ref.read(namesRegistrationProvider);
    }
    if (registration.draftPhase == 'awaiting_bond') {
      final ownAddress = ref.read(accountProvider).value?.activeAddress;
      if (ownAddress == null || ownAddress.isEmpty) return;
      context.go(
        '/send',
        extra: SendPrefillArgs(
          id: 'names-bond-${DateTime.now().microsecondsSinceEpoch}',
          source: 'names-bond',
          address: ownAddress,
          amountText: '1',
          label: 'Prepare Coppice Names bond',
          message:
              'Send exactly 1 ZEC to this wallet. After confirmation, return '
              'to Names and continue this registration.',
        ),
      );
      return;
    }
    if (registration.draftPhase != 'bond_reserved') return;
    final review = await notifier.begin(
      name: registration.draftName ?? _registrationNameController.text,
      paymentAddress:
          registration.draftPaymentAddress ??
          _registrationAddressController.text,
    );
    if (!mounted || review == null) return;
    await context.push('/send/review', extra: review);
  }

  Future<void> _revealName(String name) async {
    setState(() {
      _managedNameInFlight = name;
      _managedNameError = null;
    });
    final notifier = ref.read(managedNamesProvider.notifier);
    String? error;
    final review = await notifier.beginReveal(name);
    if (review == null) {
      error = notifier.lastRevealError;
    } else {
      if (!mounted) {
        await discardSendProposal(
          proposalId: review.proposalId,
          sendFlowId: review.sendFlowId,
          logContext: 'NamesReveal(disposed)',
        );
        return;
      }
      try {
        final reviewRoute = context.push('/send/review', extra: review);
        // The generic send flow completes with `go('/names')`, which replaces
        // the route instead of popping this pushed route. Do not keep every
        // managed-name action disabled while waiting on a Future that may
        // therefore never complete. The review screen owns the proposal as
        // soon as push succeeds.
        if (mounted) {
          setState(() => _managedNameInFlight = null);
        }
        await reviewRoute;
      } catch (routeError) {
        // If route construction fails before SendReviewScreen can own the
        // capability, release it through the same idempotent generic path.
        await discardSendProposal(
          proposalId: review.proposalId,
          sendFlowId: review.sendFlowId,
          logContext: 'NamesReveal(route-failure)',
        );
        error = routeError.toString();
      }
    }
    if (!mounted) return;
    // Refresh after returning from review/status and on begin failure: the
    // Keep lifecycle state in sync with the latest chain tip after review or
    // a failed proposal attempt.
    unawaited(ref.read(managedNamesProvider.notifier).refresh());
    setState(() {
      _managedNameInFlight = null;
      _managedNameError = error;
    });
  }

  Future<void> _resumeRegistration(rust_names.ApiManagedName item) async {
    final address = item.paymentAddress;
    if (address == null || address.isEmpty) return;
    _registrationNameController.text = item.name;
    _registrationAddressController.text = address;
    ref
        .read(namesRegistrationProvider.notifier)
        .resumeDraft(
          name: item.name,
          paymentAddress: address,
          phase: item.phase,
        );
    await _registerName();
  }

  Future<void> _discardRegistration(rust_names.ApiManagedName item) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Start ${item.name}.zec over?'),
        content: const Text(
          'This discards the local unfinished workflow. It does not alter any '
          'canonical Names history. A broadcast COMMIT keeps its normal '
          'height-bounded bond lock; it is never force-unlocked here.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          TextButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Start over'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;
    setState(() {
      _managedNameInFlight = item.name;
      _managedNameError = null;
    });
    final error = await ref
        .read(managedNamesProvider.notifier)
        .discardUncompletedRegistration(item.name);
    if (!mounted) return;
    setState(() {
      _managedNameInFlight = null;
      _managedNameError = error;
    });
  }

  Future<void> _manageName(
    rust_names.ApiManagedName item,
    String action,
  ) async {
    String? address;
    if (action == 'update') {
      final controller = TextEditingController(text: item.paymentAddress ?? '');
      address = await showDialog<String>(
        context: context,
        builder: (context) => AlertDialog(
          title: Text('Update ${item.name}.zec'),
          content: TextField(
            controller: controller,
            decoration: const InputDecoration(labelText: 'New payment address'),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Cancel'),
            ),
            TextButton(
              onPressed: () => Navigator.pop(context, controller.text.trim()),
              child: const Text('Update'),
            ),
          ],
        ),
      );
      controller.dispose();
      if (address == null || address.isEmpty) return;
    } else if (action == 'release') {
      final confirmed = await showDialog<bool>(
        context: context,
        builder: (context) => AlertDialog(
          title: Text('Release ${item.name}.zec?'),
          content: const Text(
            'The name will stop resolving and enter the protocol reuse delay.',
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context, false),
              child: const Text('Cancel'),
            ),
            TextButton(
              onPressed: () => Navigator.pop(context, true),
              child: const Text('Release'),
            ),
          ],
        ),
      );
      if (confirmed != true) return;
    }
    setState(() {
      _managedNameInFlight = item.name;
      _managedNameError = null;
    });
    final error = await ref
        .read(managedNamesProvider.notifier)
        .manage(item.name, action, paymentAddress: address);
    if (!mounted) return;
    setState(() {
      _managedNameInFlight = null;
      _managedNameError = error;
    });
  }

  @override
  Widget build(BuildContext context) {
    ref.listen<AsyncValue<SyncState>>(syncProvider, (previous, next) {
      final sync = next.asData?.value;
      if (sync == null) return;
      // Names replay is persisted by the Rust sync engine before it reports
      // successful completion. Explicitly refresh these independent sidecar
      // reads on that boundary: watching progress alone can otherwise leave a
      // tab that was already open showing a pre-COMMIT tip until restart.
      final previousCompletedAt = previous?.asData?.value.lastSyncCompletedAt;
      if (sync.isSyncComplete &&
          sync.lastSyncCompletedAt != null &&
          sync.lastSyncCompletedAt != previousCompletedAt) {
        // Refresh in deterministic order: status first, then managed data,
        // then the local draft phase. This avoids reading a sidecar while a
        // completed wallet sync is still publishing its Names checkpoint.
        unawaited(_refreshNamesAfterCompletedSync());
      }
    });
    final colors = context.colors;
    final statusAsync = ref.watch(namesStatusProvider);
    final profile = ref.watch(namesDeploymentProfileProvider);
    final action = ref.watch(namesActionProvider);
    final lookup = ref.watch(nameLookupProvider);
    final registration = ref.watch(namesRegistrationProvider);
    final managedNames = ref.watch(managedNamesProvider);
    final horizontalPadding = widget.showDesktopChrome
        ? AppSpacing.md
        : AppSpacing.sm;
    final lookupAvailable = _namesLookupAvailable(statusAsync, profile);
    final registrationAvailable = _namesRegistrationAvailable(
      statusAsync,
      profile,
    );

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (widget.showDesktopChrome)
          const AppPaneToolbar(backLinkMinWidth: 60),
        Padding(
          padding: EdgeInsets.fromLTRB(
            horizontalPadding,
            widget.showDesktopChrome ? AppSpacing.xs : AppSpacing.md,
            horizontalPadding,
            AppSpacing.sm,
          ),
          child: _NamesHeader(colors: colors),
        ),
        Expanded(
          child: SingleChildScrollView(
            padding: EdgeInsets.fromLTRB(
              horizontalPadding,
              0,
              horizontalPadding,
              AppSpacing.md,
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                _DeploymentStatusCard(
                  profile: profile,
                  statusAsync: statusAsync,
                  action: action,
                  onConfigure: () => ref
                      .read(namesStatusProvider.notifier)
                      .configureWithDeploymentProfile(),
                  onBootstrap: () => ref
                      .read(namesStatusProvider.notifier)
                      .bootstrapFromActiveEndpoint(),
                  onRetry: () =>
                      ref.read(namesStatusProvider.notifier).refresh(),
                ),
                const SizedBox(height: AppSpacing.md),
                _NameLookupCard(
                  controller: _nameController,
                  lookup: lookup,
                  available: lookupAvailable,
                  unavailableMessage: _namesLookupUnavailableMessage(
                    statusAsync,
                    profile,
                  ),
                  onResolve: _resolveName,
                  onSend: _sendToResolution,
                ),
                const SizedBox(height: AppSpacing.md),
                _RegistrationCard(
                  nameController: _registrationNameController,
                  addressController: _registrationAddressController,
                  state: registration,
                  available: registrationAvailable,
                  onRegister: _registerName,
                ),
                const SizedBox(height: AppSpacing.md),
                _ManagedNamesCard(
                  names: managedNames,
                  bootstrapRequired:
                      statusAsync.value?.state == 'needs_bootstrap',
                  onBootstrap: () => ref
                      .read(namesStatusProvider.notifier)
                      .bootstrapFromActiveEndpoint(),
                  inFlightName: _managedNameInFlight,
                  error: _managedNameError,
                  onReveal: _revealName,
                  onResumeRegistration: _resumeRegistration,
                  onDiscardRegistration: _discardRegistration,
                  onManage: _manageName,
                  onRefresh: () =>
                      ref.read(managedNamesProvider.notifier).refresh(),
                ),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

/// Exact-name lookup is independent of full bootstrap on the Rust host, so
/// it is offered whenever a deployment is configured and the sidecar is not
/// corrupt.
bool _namesLookupAvailable(
  AsyncValue<rust_names.ApiNamesWalletStatus?> statusAsync,
  NamesDeploymentProfile? profile,
) {
  final status = statusAsync.value;
  if (profile == null || status == null) return false;
  return status.state == 'ready' || status.state == 'needs_bootstrap';
}

bool _namesRegistrationAvailable(
  AsyncValue<rust_names.ApiNamesWalletStatus?> statusAsync,
  NamesDeploymentProfile? profile,
) {
  return profile != null && statusAsync.value?.state == 'ready';
}

String _namesLookupUnavailableMessage(
  AsyncValue<rust_names.ApiNamesWalletStatus?> statusAsync,
  NamesDeploymentProfile? profile,
) {
  if (profile == null) {
    return 'Coppice Names is not configured for this network.';
  }
  final status = statusAsync.value;
  if (status == null) {
    return 'Unlock your wallet to look up names.';
  }
  if (status.state == 'corrupt') {
    return 'Names state is unusable — see the status above.';
  }
  return 'Enable Coppice Names to look up names.';
}

class _NamesHeader extends StatelessWidget {
  const _NamesHeader({required this.colors});

  final AppColors colors;

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        AppIcon(AppIcons.globe, size: 24, color: colors.icon.accent),
        const SizedBox(width: AppSpacing.xs),
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                'Coppice Names',
                style: AppTypography.headlineSmall.copyWith(
                  color: colors.text.accent,
                ),
              ),
              Text(
                'Resolve .zec names to shielded payment addresses.',
                style: AppTypography.bodySmall.copyWith(
                  color: colors.text.secondary,
                ),
              ),
            ],
          ),
        ),
      ],
    );
  }
}

class _DeploymentStatusCard extends StatelessWidget {
  const _DeploymentStatusCard({
    required this.profile,
    required this.statusAsync,
    required this.action,
    required this.onConfigure,
    required this.onBootstrap,
    required this.onRetry,
  });

  final NamesDeploymentProfile? profile;
  final AsyncValue<rust_names.ApiNamesWalletStatus?> statusAsync;
  final NamesActionState action;
  final VoidCallback onConfigure;
  final VoidCallback onBootstrap;
  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    final Widget body;
    if (statusAsync.isLoading && !statusAsync.hasValue) {
      body = const Padding(
        padding: EdgeInsets.symmetric(vertical: AppSpacing.md),
        child: Center(
          child: SizedBox(
            width: 18,
            height: 18,
            child: CircularProgressIndicator(strokeWidth: 2),
          ),
        ),
      );
    } else if (profile == null) {
      body = const _DeploymentMissingCard();
    } else if (statusAsync.hasError) {
      body = Column(
        key: const ValueKey('names_status_error'),
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const _NamesMessage(
            icon: AppIcons.warning,
            title: 'Names unavailable',
            message: 'The wallet could not read Coppice Names state.',
          ),
          const SizedBox(height: AppSpacing.sm),
          Align(
            alignment: Alignment.centerLeft,
            child: AppButton(
              key: const ValueKey('names_status_retry_button'),
              variant: AppButtonVariant.secondary,
              size: AppButtonSize.medium,
              onPressed: onRetry,
              child: const Text('Retry'),
            ),
          ),
        ],
      );
    } else if (statusAsync.hasValue && statusAsync.value != null) {
      body = _buildStateBody(context, statusAsync.value!);
    } else {
      body = _NamesMessage(
        icon: AppIcons.lock,
        title: 'Wallet locked',
        message: 'Unlock your wallet to use Coppice Names.',
      );
    }
    return _NamesCard(
      key: const ValueKey('names_status_card'),
      children: [
        body,
        if (action.error != null) ...[
          const SizedBox(height: AppSpacing.xs),
          Text(
            action.error!,
            key: const ValueKey('names_action_error'),
            style: AppTypography.bodySmall.copyWith(
              color: colors.text.destructive,
            ),
          ),
        ],
      ],
    );
  }

  Widget _buildStateBody(
    BuildContext context,
    rust_names.ApiNamesWalletStatus status,
  ) {
    final colors = context.colors;
    switch (status.state) {
      case 'ready':
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _LifecycleChip(
              key: const ValueKey('names_state_ready'),
              label: 'Ready',
              color: colors.text.success,
              icon: AppIcons.checkCircle,
            ),
            const SizedBox(height: AppSpacing.xs),
            _InfoRow(label: 'Deployment', value: profile!.label),
            _InfoRow(label: 'Chain tip', value: status.tipHeight.toString()),
            _InfoRow(
              label: 'Names activation height',
              value: status.namesActivationHeight.toString(),
            ),
            if (status.oldestRewindHeight > BigInt.zero)
              _InfoRow(
                label: 'Oldest rewind height',
                value: status.oldestRewindHeight.toString(),
              ),
          ],
        );
      case 'needs_bootstrap':
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _LifecycleChip(
              key: const ValueKey('names_state_needs_bootstrap'),
              label: 'Ready to bootstrap',
              color: colors.text.warning,
              icon: AppIcons.sync,
            ),
            const SizedBox(height: AppSpacing.xs),
            _InfoRow(label: 'Deployment', value: profile!.label),
            Text(
              'Names streams the chain from the active endpoint once to '
              'build authenticated state. Exact-name lookup works before '
              'bootstrap completes.',
              style: AppTypography.bodySmall.copyWith(
                color: colors.text.secondary,
              ),
            ),
            const SizedBox(height: AppSpacing.sm),
            Align(
              alignment: Alignment.centerLeft,
              child: AppButton(
                key: const ValueKey('names_bootstrap_button'),
                variant: AppButtonVariant.primary,
                size: AppButtonSize.medium,
                onPressed: action.inFlight == null ? onBootstrap : null,
                trailing: action.inFlight == 'bootstrap'
                    ? const _InlineSpinner()
                    : null,
                child: Text(
                  action.inFlight == 'bootstrap'
                      ? 'Bootstrapping…'
                      : 'Bootstrap Names',
                ),
              ),
            ),
          ],
        );
      case 'corrupt':
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _LifecycleChip(
              key: const ValueKey('names_state_corrupt'),
              label: 'State unusable',
              color: colors.text.destructive,
              icon: AppIcons.warning,
            ),
            const SizedBox(height: AppSpacing.xs),
            Text(
              status.message,
              style: AppTypography.bodySmall.copyWith(
                color: colors.text.secondary,
              ),
            ),
          ],
        );
      case 'disabled':
      default:
        return _DeploymentEnableSection(
          profile: profile!,
          configuring: action.inFlight == 'configure',
          onConfigure: onConfigure,
        );
    }
  }
}

/// The explicit, per-network deployment gate. When no profile exists the
/// UI explains why and offers no way to invent parameters.
class _DeploymentMissingCard extends StatelessWidget {
  const _DeploymentMissingCard();

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return Column(
      key: const ValueKey('names_deployment_missing'),
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _LifecycleChip(
          label: 'Not configured',
          color: colors.text.warning,
          icon: AppIcons.warning,
        ),
        const SizedBox(height: AppSpacing.xs),
        Text(
          'No Coppice/Names deployment is configured for this network.',
          style: AppTypography.bodyMediumStrong.copyWith(
            color: colors.text.primary,
          ),
        ),
        const SizedBox(height: AppSpacing.xxs),
        Text(
          'Deployment parameters must be supplied explicitly per network. '
          'The wallet will not reuse another network\u2019s identity, so '
          'Coppice Names stays disabled here until a deployment profile is '
          'shipped for this network.',
          style: AppTypography.bodySmall.copyWith(color: colors.text.secondary),
        ),
      ],
    );
  }
}

class _DeploymentEnableSection extends StatelessWidget {
  const _DeploymentEnableSection({
    required this.profile,
    required this.configuring,
    required this.onConfigure,
  });

  final NamesDeploymentProfile profile;
  final bool configuring;
  final VoidCallback onConfigure;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return Column(
      key: const ValueKey('names_deployment_enable'),
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Expanded(
              child: Text(
                'Enable Coppice Names on this wallet',
                style: AppTypography.bodyMediumStrong.copyWith(
                  color: colors.text.primary,
                ),
              ),
            ),
            _TestChainBadge(profile: profile),
          ],
        ),
        const SizedBox(height: AppSpacing.xxs),
        Text(
          'The wallet will record the deployment parameters below '
          'permanently. They are explicit for this network and are never '
          'taken from another network\u2019s identity.',
          style: AppTypography.bodySmall.copyWith(color: colors.text.secondary),
        ),
        const SizedBox(height: AppSpacing.sm),
        _DeploymentParameterGrid(profile: profile),
        const SizedBox(height: AppSpacing.sm),
        Align(
          alignment: Alignment.centerLeft,
          child: AppButton(
            key: const ValueKey('names_configure_button'),
            variant: AppButtonVariant.primary,
            size: AppButtonSize.medium,
            onPressed: configuring ? null : onConfigure,
            trailing: configuring ? const _InlineSpinner() : null,
            child: Text(configuring ? 'Enabling…' : 'Enable Coppice Names'),
          ),
        ),
      ],
    );
  }
}

class _DeploymentParameterGrid extends StatelessWidget {
  const _DeploymentParameterGrid({required this.profile});

  final NamesDeploymentProfile profile;

  @override
  Widget build(BuildContext context) {
    return _NamesCard(
      backgroundColor: context.colors.background.ground,
      boxShadow: false,
      children: [
        _InfoRow(label: 'Profile', value: profile.label),
        _InfoRow(label: 'Network domain', value: profile.networkDomain),
        _InfoRow(
          label: 'Activation height',
          value: profile.activationHeight.toString(),
        ),
        _InfoRow(
          label: 'Daily schedule (blocks)',
          value: kNamesEpochBlocks.toString(),
        ),
        _InfoRow(
          label: 'Name window (blocks)',
          value: kNamesWindowBlocks.toString(),
        ),
        _InfoRow(
          label: 'Commit maturity (blocks)',
          value: kNamesCommitMaturityBlocks.toString(),
        ),
        _InfoRow(
          label: 'Commit TTL (blocks)',
          value: kNamesCommitTtlBlocks.toString(),
        ),
        _InfoRow(
          label: 'Lease duration (blocks)',
          value: kNamesLeaseBlocks.toString(),
        ),
        _InfoRow(
          label: 'Cooldown (blocks)',
          value: kNamesCooldownBlocks.toString(),
        ),
        _InfoRow(
          label: 'Bond (zatoshis)',
          value: kNamesBondZatoshis.toString(),
        ),
        _InfoRow(
          label: 'Retention (blocks)',
          value: profile.retentionBlocks.toString(),
        ),
        _InfoRow(
          label: 'Rendezvous IVK',
          value: _compactHex(profile.rendezvousIvkHex),
        ),
        _InfoRow(
          label: 'Rendezvous receiver',
          value: _compactHex(profile.rendezvousReceiverHex),
        ),
      ],
    );
  }
}

String _compactHex(String hex) {
  if (hex.length <= 24) return hex;
  return '${hex.substring(0, 12)}…${hex.substring(hex.length - 12)}';
}

class _RegistrationCard extends StatelessWidget {
  const _RegistrationCard({
    required this.nameController,
    required this.addressController,
    required this.state,
    required this.available,
    required this.onRegister,
  });

  final TextEditingController nameController;
  final TextEditingController addressController;
  final NamesRegistrationState state;
  final bool available;
  final VoidCallback onRegister;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    final hasInput =
        nameController.text.trim().isNotEmpty &&
        addressController.text.trim().isNotEmpty;
    final bond = state.bondStatus;
    final buttonLabel = switch (state.draftPhase) {
      'awaiting_bond' => 'Prepare 1 ZEC note',
      'bond_reserved' => 'Continue registration',
      _ => switch (bond?.state) {
        'needs_preparation' => 'Prepare 1 ZEC note',
        'insufficient_funds' => 'Insufficient ZEC',
        _ => 'Register name',
      },
    };
    return _NamesCard(
      key: const ValueKey('names_registration_card'),
      children: [
        Text(
          'Register a name',
          style: AppTypography.bodyMediumStrong.copyWith(
            color: colors.text.primary,
          ),
        ),
        const SizedBox(height: AppSpacing.xxs),
        Text(
          'Registration uses an exact 1 ZEC refundable bond. If the wallet '
          'does not have that denomination, it will prepare one first. After '
          'COMMIT is accepted, approve REVEAL before its commitment expires.',
          style: AppTypography.bodySmall.copyWith(color: colors.text.secondary),
        ),
        const SizedBox(height: AppSpacing.sm),
        AppTextField(
          key: const ValueKey('names_registration_name_field'),
          label: 'Name',
          controller: nameController,
          hintText: 'alice',
          enabled: available && !state.inFlight,
          inlineSuffixText: '.zec',
          onChanged: (_) {},
        ),
        const SizedBox(height: AppSpacing.sm),
        AppTextField(
          key: const ValueKey('names_registration_address_field'),
          label: 'Payment address',
          controller: addressController,
          enabled: available && !state.inFlight,
          onChanged: (_) {},
        ),
        if (bond != null) ...[
          const SizedBox(height: AppSpacing.xs),
          Text(
            switch (bond.state) {
              'ready' => 'Exact 1 ZEC bond note ready.',
              'needs_preparation' =>
                'The wallet will open a prefilled 1 ZEC self-transfer.',
              _ => 'At least 1 ZEC plus transaction fees is required.',
            },
            style: AppTypography.bodySmall.copyWith(
              color: bond.state == 'ready'
                  ? colors.text.success
                  : colors.text.secondary,
            ),
          ),
        ],
        if (state.draftPhase == 'awaiting_bond') ...[
          const SizedBox(height: AppSpacing.xs),
          Text(
            'Registration is saved. Confirm the 1 ZEC self-transfer, then return here.',
            style: AppTypography.bodySmall.copyWith(
              color: colors.text.secondary,
            ),
          ),
        ] else if (state.draftPhase == 'bond_reserved') ...[
          const SizedBox(height: AppSpacing.xs),
          Text(
            'The exact bond note is reserved for this name. Continue to review the COMMIT.',
            style: AppTypography.bodySmall.copyWith(color: colors.text.success),
          ),
        ],
        if (state.error != null) ...[
          const SizedBox(height: AppSpacing.xs),
          Text(
            state.error!,
            key: const ValueKey('names_registration_error'),
            style: AppTypography.bodySmall.copyWith(
              color: colors.text.destructive,
            ),
          ),
        ],
        const SizedBox(height: AppSpacing.sm),
        Align(
          alignment: Alignment.centerLeft,
          child: AppButton(
            key: const ValueKey('names_registration_button'),
            variant: AppButtonVariant.primary,
            size: AppButtonSize.medium,
            onPressed:
                available &&
                    hasInput &&
                    !state.inFlight &&
                    bond?.state != 'insufficient_funds'
                ? onRegister
                : null,
            child: state.inFlight ? const _InlineSpinner() : Text(buttonLabel),
          ),
        ),
      ],
    );
  }
}

class _ManagedNamesCard extends StatelessWidget {
  const _ManagedNamesCard({
    required this.names,
    required this.bootstrapRequired,
    required this.onBootstrap,
    required this.inFlightName,
    required this.error,
    required this.onReveal,
    required this.onResumeRegistration,
    required this.onDiscardRegistration,
    required this.onManage,
    required this.onRefresh,
  });

  final AsyncValue<List<rust_names.ApiManagedName>> names;
  final bool bootstrapRequired;
  final VoidCallback onBootstrap;
  final String? inFlightName;
  final String? error;
  final ValueChanged<String> onReveal;
  final ValueChanged<rust_names.ApiManagedName> onResumeRegistration;
  final ValueChanged<rust_names.ApiManagedName> onDiscardRegistration;
  final void Function(rust_names.ApiManagedName, String) onManage;
  final VoidCallback onRefresh;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return _NamesCard(
      key: const ValueKey('managed_names_card'),
      children: [
        Row(
          children: [
            Expanded(
              child: Text(
                'Your names',
                style: AppTypography.bodyMediumStrong.copyWith(
                  color: colors.text.primary,
                ),
              ),
            ),
            AppIconHoverButton(
              icon: AppIcons.sync,
              semanticLabel: 'Refresh managed names',
              onTap: onRefresh,
            ),
          ],
        ),
        const SizedBox(height: AppSpacing.xs),
        if (bootstrapRequired)
          Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text(
                'Bootstrap Names to load registration workflows.',
                style: AppTypography.bodySmall.copyWith(
                  color: colors.text.secondary,
                ),
              ),
              const SizedBox(height: AppSpacing.xs),
              Align(
                alignment: Alignment.centerLeft,
                child: AppButton(
                  key: const ValueKey('names_managed_bootstrap_button'),
                  variant: AppButtonVariant.secondary,
                  size: AppButtonSize.medium,
                  onPressed: onBootstrap,
                  child: const Text('Bootstrap Names'),
                ),
              ),
            ],
          )
        else
          names.when(
            loading: () => const Center(child: _InlineSpinner()),
            error: (error, stackTrace) => Text(
              'Managed names could not be loaded.',
              style: AppTypography.bodySmall.copyWith(
                color: colors.text.destructive,
              ),
            ),
            data: (items) {
              if (items.isEmpty) {
                return Text(
                  'No registration workflows for this account.',
                  style: AppTypography.bodySmall.copyWith(
                    color: colors.text.secondary,
                  ),
                );
              }
              return Column(
                children: items
                    .map(
                      (item) => Padding(
                        key: ValueKey('managed_name_row_${item.name}'),
                        padding: const EdgeInsets.only(bottom: AppSpacing.xs),
                        child: Row(
                          children: [
                            Expanded(
                              child: Column(
                                crossAxisAlignment: CrossAxisAlignment.start,
                                children: [
                                  Text(
                                    '${item.name}.zec',
                                    style: AppTypography.bodyMediumStrong
                                        .copyWith(color: colors.text.primary),
                                  ),
                                  Text(
                                    _managedPhaseLabel(item.phase),
                                    style: AppTypography.bodySmall.copyWith(
                                      color: colors.text.secondary,
                                    ),
                                  ),
                                  if (item.phase == 'commit_accepted' &&
                                      item.commitBlocksRemaining != null)
                                    Text(
                                      '${item.commitBlocksRemaining} blocks remaining before COMMIT expiry '
                                      '(height ${item.commitExpiryHeight})',
                                      style: AppTypography.bodySmall.copyWith(
                                        color: colors.text.warning,
                                      ),
                                    ),
                                  if (item.phase == 'commit_accepted')
                                    Text(
                                      item.revealWindowOpen
                                          ? 'REVEAL window is open through height '
                                                '${item.revealWindowEnd - BigInt.one}'
                                          : 'REVEAL window opens at height '
                                                '${item.revealWindowStart} '
                                                '(${item.revealBlocksUntil} blocks)',
                                      style: AppTypography.bodySmall.copyWith(
                                        color: item.revealWindowOpen
                                            ? colors.text.success
                                            : colors.text.secondary,
                                      ),
                                    ),
                                ],
                              ),
                            ),
                            if (item.phase == 'commit_accepted' &&
                                item.revealWindowOpen)
                              AppButton(
                                key: ValueKey(
                                  'names_reveal_button_${item.name}',
                                ),
                                variant: AppButtonVariant.secondary,
                                size: AppButtonSize.medium,
                                onPressed: inFlightName == null
                                    ? () => onReveal(item.name)
                                    : null,
                                child: inFlightName == item.name
                                    ? const _InlineSpinner()
                                    : const Text('Reveal now'),
                              ),
                            if (item.phase == 'awaiting_bond' ||
                                item.phase == 'bond_reserved')
                              AppButton(
                                key: ValueKey(
                                  'names_resume_registration_${item.name}',
                                ),
                                variant: AppButtonVariant.secondary,
                                size: AppButtonSize.medium,
                                onPressed: inFlightName == null
                                    ? () => onResumeRegistration(item)
                                    : null,
                                child: Text(
                                  item.phase == 'bond_reserved'
                                      ? 'Continue'
                                      : 'Prepare bond',
                                ),
                              ),
                            if (item.phase == 'commit_proposed' ||
                                item.phase == 'commit_broadcast' ||
                                item.phase == 'window_missed' ||
                                item.phase == 'commit_expired')
                              AppButton(
                                variant: AppButtonVariant.secondary,
                                size: AppButtonSize.medium,
                                onPressed: inFlightName == null
                                    ? () => onDiscardRegistration(item)
                                    : null,
                                child: const Text('Start over'),
                              ),
                            if (item.phase == 'active')
                              PopupMenuButton<String>(
                                enabled: inFlightName == null,
                                tooltip: 'Manage ${item.name}.zec',
                                onSelected: (action) => onManage(item, action),
                                itemBuilder: (context) => const [
                                  PopupMenuItem(
                                    value: 'update',
                                    child: Text('Update address'),
                                  ),
                                  PopupMenuItem(
                                    value: 'renew',
                                    child: Text('Renew lease'),
                                  ),
                                  PopupMenuItem(
                                    value: 'release',
                                    child: Text('Release name'),
                                  ),
                                ],
                                child: inFlightName == item.name
                                    ? const _InlineSpinner()
                                    : const AppIcon(AppIcons.options),
                              ),
                          ],
                        ),
                      ),
                    )
                    .toList(),
              );
            },
          ),
        if (error != null) ...[
          const SizedBox(height: AppSpacing.sm),
          Container(
            key: const ValueKey('managed_names_error'),
            padding: const EdgeInsets.all(AppSpacing.sm),
            decoration: BoxDecoration(
              color: colors.background.ground,
              borderRadius: BorderRadius.circular(AppRadii.small),
              border: Border.all(
                color: colors.text.destructive.withValues(alpha: 0.4),
              ),
            ),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                AppIcon(
                  AppIcons.warning,
                  size: 16,
                  color: colors.text.destructive,
                ),
                const SizedBox(width: AppSpacing.xs),
                Expanded(
                  child: Text(
                    error!,
                    style: AppTypography.bodySmall.copyWith(
                      color: colors.text.destructive,
                    ),
                  ),
                ),
              ],
            ),
          ),
        ],
      ],
    );
  }
}

String _managedPhaseLabel(String phase) => switch (phase) {
  'awaiting_bond' => 'Awaiting exact 1 ZEC bond note',
  'bond_reserved' => 'Exact 1 ZEC bond reserved — continue registration',
  'commit_proposed' => 'COMMIT proposal awaiting wallet confirmation',
  'commit_broadcast' => 'COMMIT broadcast — awaiting canonical Names replay',
  'commit_accepted' => 'COMMIT accepted — REVEAL is available',
  'window_missed' => 'Registration window missed — start again',
  'commit_expired' => 'COMMIT expired before REVEAL',
  'reveal_broadcast' => 'REVEAL broadcast — awaiting confirmation',
  'active' => 'Active',
  'cooldown' => 'Expired — reserved for the previous owner',
  'claimable' => 'Available to register',
  _ => phase.replaceAll('_', ' '),
};

class _NameLookupCard extends StatelessWidget {
  const _NameLookupCard({
    required this.controller,
    required this.lookup,
    required this.available,
    required this.unavailableMessage,
    required this.onResolve,
    required this.onSend,
  });

  final TextEditingController controller;
  final NameLookupState lookup;
  final bool available;
  final String unavailableMessage;
  final VoidCallback onResolve;
  final ValueChanged<ZecNameResolution> onSend;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    final resolution = lookup.resolution;
    final hasText = controller.text.trim().isNotEmpty;
    return _NamesCard(
      key: const ValueKey('names_lookup_card'),
      children: [
        Text(
          'Look up a name',
          style: AppTypography.bodyMediumStrong.copyWith(
            color: colors.text.primary,
          ),
        ),
        const SizedBox(height: AppSpacing.xs),
        AppTextField(
          key: const ValueKey('names_lookup_field'),
          label: 'Name',
          showLabel: false,
          controller: controller,
          enabled: available,
          hintText: available ? 'e.g. alice.zec' : unavailableMessage,
          leading: AppIcon(
            AppIcons.search,
            size: 20,
            color: colors.icon.regular,
          ),
          leadingSlotWidth: 32,
          onChanged: (_) {},
          onSubmitted: (_) => onResolve(),
          textStyle: AppTypography.codeMedium.copyWith(
            color: colors.text.accent,
          ),
        ),
        const SizedBox(height: AppSpacing.sm),
        Align(
          alignment: Alignment.centerLeft,
          child: AppButton(
            key: const ValueKey('names_resolve_button'),
            variant: AppButtonVariant.secondary,
            size: AppButtonSize.medium,
            onPressed: available && hasText ? onResolve : null,
            child: const Text('Resolve'),
          ),
        ),
        if (lookup.error != null) ...[
          const SizedBox(height: AppSpacing.sm),
          Text(
            lookup.error!,
            key: const ValueKey('names_lookup_error'),
            style: AppTypography.bodySmall.copyWith(
              color: colors.text.destructive,
            ),
          ),
        ],
        if (resolution != null) ...[
          const SizedBox(height: AppSpacing.sm),
          _ResolutionResult(
            resolution: resolution,
            onSend: () => onSend(resolution),
          ),
        ],
      ],
    );
  }
}

class _ResolutionResult extends StatelessWidget {
  const _ResolutionResult({required this.resolution, required this.onSend});

  final ZecNameResolution resolution;
  final VoidCallback onSend;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return Container(
      key: const ValueKey('names_lookup_result'),
      padding: const EdgeInsets.all(AppSpacing.sm),
      decoration: BoxDecoration(
        color: colors.background.ground,
        borderRadius: BorderRadius.circular(AppRadii.small),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  resolution.name,
                  style: AppTypography.labelLarge.copyWith(
                    color: colors.text.accent,
                  ),
                ),
              ),
              _LifecycleChip(
                label: 'Active',
                color: colors.text.success,
                icon: AppIcons.checkCircle,
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.xxs),
          _CopyableAddressRow(address: resolution.paymentAddress),
          if (resolution.leaseExpiryHeight != null)
            _InfoRow(
              label: 'Lease expiry height',
              value: resolution.leaseExpiryHeight.toString(),
            ),
          _InfoRow(
            label: 'Resolved at height',
            value: resolution.tipHeight.toString(),
          ),
          const SizedBox(height: AppSpacing.xs),
          Align(
            alignment: Alignment.centerLeft,
            child: AppButton(
              key: const ValueKey('names_send_to_address_button'),
              variant: AppButtonVariant.primary,
              size: AppButtonSize.medium,
              onPressed: onSend,
              child: const Text('Send to this address'),
            ),
          ),
        ],
      ),
    );
  }
}

class _CopyableAddressRow extends StatelessWidget {
  const _CopyableAddressRow({required this.address});

  final String address;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                'Payment address',
                style: AppTypography.labelSmall.copyWith(
                  color: colors.text.muted,
                ),
              ),
              const SizedBox(height: AppSpacing.xxs),
              SelectionArea(
                child: Text(
                  address,
                  style: AppTypography.codeMedium.copyWith(
                    color: colors.text.accent,
                  ),
                ),
              ),
            ],
          ),
        ),
        const SizedBox(width: AppSpacing.xs),
        AppIconHoverButton(
          key: const ValueKey('names_copy_address_button'),
          icon: AppIcons.copy,
          semanticLabel: 'Copy payment address',
          iconSize: 16,
          iconColor: colors.icon.regular,
          onTap: () => copyTextWithToast(
            context,
            text: address,
            toastMessage: 'Address copied',
          ),
        ),
      ],
    );
  }
}

class _NamesCard extends StatelessWidget {
  const _NamesCard({
    required this.children,
    this.backgroundColor,
    this.boxShadow = true,
    super.key,
  });

  final List<Widget> children;
  final Color? backgroundColor;
  final bool boxShadow;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return Container(
      padding: const EdgeInsets.all(AppSpacing.sm),
      decoration: BoxDecoration(
        color: backgroundColor ?? colors.background.raised,
        borderRadius: BorderRadius.circular(AppRadii.large),
        boxShadow: boxShadow ? appSurfaceShadow(colors) : null,
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: children,
      ),
    );
  }
}

class _NamesMessage extends StatelessWidget {
  const _NamesMessage({
    required this.icon,
    required this.title,
    required this.message,
  });

  final String icon;
  final String title;
  final String message;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          title,
          style: AppTypography.bodyMediumStrong.copyWith(
            color: colors.text.primary,
          ),
        ),
        const SizedBox(height: AppSpacing.xxs),
        Text(
          message,
          style: AppTypography.bodySmall.copyWith(color: colors.text.secondary),
        ),
      ],
    );
  }
}

class _LifecycleChip extends StatelessWidget {
  const _LifecycleChip({
    required this.label,
    required this.color,
    required this.icon,
    super.key,
  });

  final String label;
  final Color color;
  final String icon;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        AppIcon(icon, size: 16, color: color),
        const SizedBox(width: AppSpacing.xxs),
        Text(label, style: AppTypography.labelLarge.copyWith(color: color)),
      ],
    );
  }
}

class _TestChainBadge extends StatelessWidget {
  const _TestChainBadge({required this.profile});

  final NamesDeploymentProfile profile;

  @override
  Widget build(BuildContext context) {
    if (profile.isProduction) return const SizedBox.shrink();
    final colors = context.colors;
    return Container(
      key: const ValueKey('names_test_chain_badge'),
      padding: const EdgeInsets.symmetric(horizontal: AppSpacing.xs),
      decoration: BoxDecoration(
        color: colors.background.neutralSubtleOpacity,
        borderRadius: BorderRadius.circular(AppRadii.full),
      ),
      child: Text(
        'Test chain',
        style: AppTypography.labelSmall.copyWith(color: colors.text.secondary),
      ),
    );
  }
}

class _InfoRow extends StatelessWidget {
  const _InfoRow({required this.label, required this.value});

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: AppSpacing.xxs),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 168,
            child: Text(
              label,
              style: AppTypography.bodySmall.copyWith(color: colors.text.muted),
            ),
          ),
          Expanded(
            child: Text(
              value,
              style: AppTypography.bodySmall.copyWith(
                color: colors.text.accent,
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _InlineSpinner extends StatelessWidget {
  const _InlineSpinner();

  @override
  Widget build(BuildContext context) {
    return const SizedBox(
      width: 14,
      height: 14,
      child: CircularProgressIndicator(strokeWidth: 2),
    );
  }
}
