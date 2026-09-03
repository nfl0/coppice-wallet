import 'dart:async';
import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:go_router/go_router.dart';
import 'package:zcash_wallet/src/core/theme/app_theme.dart';
import 'package:zcash_wallet/src/features/send/services/send_flow.dart';
import 'package:zcash_wallet/src/features/names/models/names_deployment.dart';
import 'package:zcash_wallet/src/features/names/providers/names_provider.dart';
import 'package:zcash_wallet/src/features/names/screens/names_screen.dart';
import 'package:zcash_wallet/src/providers/account_provider.dart';
import 'package:zcash_wallet/src/rust/api/names.dart' as rust_names;

class _FailingNamesStatusNotifier extends NamesStatusNotifier {
  @override
  Future<rust_names.ApiNamesWalletStatus?> build() async {
    throw StateError('sidecar read failed');
  }
}

class _UninitializedNamesStatusNotifier extends NamesStatusNotifier {
  @override
  Future<rust_names.ApiNamesWalletStatus?> build() async =>
      rust_names.ApiNamesWalletStatus(
        state: 'needs_bootstrap',
        message: 'authenticated replay is required before Names operations',
        configured: true,
        tipHeight: BigInt.zero,
        namesActivationHeight: BigInt.from(1),
        oldestRewindHeight: BigInt.zero,
      );
}

class _ReviewManagedNamesNotifier extends ManagedNamesNotifier {
  static const name = 'reviewable';

  @override
  Future<List<rust_names.ApiManagedName>> build() async => [
    rust_names.ApiManagedName(
      name: name,
      phase: 'commit_accepted',
      commitment: Uint8List.fromList([1, 2, 3]),
      commitWindowStart: BigInt.from(70),
      commitWindowEnd: BigInt.from(97),
      commitBlocksUntil: BigInt.zero,
      commitWindowOpen: true,
      revealWindowStart: BigInt.from(100),
      revealWindowEnd: BigInt.from(124),
      revealBlocksUntil: BigInt.zero,
      revealWindowOpen: true,
      refreshWindowStart: null,
      refreshWindowEnd: null,
      refreshBlocksUntil: null,
      refreshWindowOpen: false,
    ),
  ];

  @override
  Future<SendReviewArgs?> beginReveal(String name) async => SendReviewArgs(
    proposalId: BigInt.from(42),
    sendFlowId: 'names-review-flow',
    proposalAccountUuid: 'software-account',
    address: 'Coppice Names REVEAL',
    addressType: 'unified',
    amountZatoshi: BigInt.from(kNamesBondZatoshis),
    feeZatoshi: BigInt.from(1234),
    needsSaplingParams: false,
    memo: 'Reveal ${name.trim().toLowerCase()}',
  );
}

class _WaitingManagedNamesNotifier extends ManagedNamesNotifier {
  @override
  Future<List<rust_names.ApiManagedName>> build() async => [
    rust_names.ApiManagedName(
      name: 'waiting',
      phase: 'commit_accepted',
      commitment: Uint8List.fromList([1, 2, 3]),
      commitWindowStart: BigInt.from(90),
      commitWindowEnd: BigInt.from(117),
      commitBlocksUntil: BigInt.zero,
      commitWindowOpen: false,
      revealWindowStart: BigInt.from(120),
      revealWindowEnd: BigInt.from(144),
      revealBlocksUntil: BigInt.from(19),
      revealWindowOpen: false,
      refreshWindowStart: null,
      refreshWindowEnd: null,
      refreshBlocksUntil: null,
      refreshWindowOpen: false,
    ),
  ];
}

class _ReviewManagementNotifier extends ManagedNamesNotifier {
  static String? capturedAction;
  static String? capturedAddress;

  @override
  Future<List<rust_names.ApiManagedName>> build() async => [
    rust_names.ApiManagedName(
      name: 'managed',
      paymentAddress: 'uregtest1old',
      phase: 'active',
      commitment: Uint8List.fromList([1, 2, 3]),
      commitWindowStart: BigInt.zero,
      commitWindowEnd: BigInt.zero,
      commitBlocksUntil: BigInt.zero,
      commitWindowOpen: false,
      revealWindowStart: BigInt.zero,
      revealWindowEnd: BigInt.zero,
      revealBlocksUntil: BigInt.zero,
      revealWindowOpen: false,
      refreshWindowStart: BigInt.from(100),
      refreshWindowEnd: BigInt.from(104),
      refreshBlocksUntil: BigInt.zero,
      refreshWindowOpen: true,
    ),
  ];

  @override
  Future<SendReviewArgs?> beginManagement(
    String name,
    String action, {
    String? paymentAddress,
  }) async {
    capturedAction = action;
    capturedAddress = paymentAddress;
    return SendReviewArgs(
      proposalId: BigInt.from(77),
      sendFlowId: 'names-management-review',
      proposalAccountUuid: 'software-account',
      address: 'Coppice Names ${action.toUpperCase()}',
      addressType: 'unified',
      amountZatoshi: BigInt.from(kNamesBondZatoshis),
      feeZatoshi: BigInt.from(5678),
      needsSaplingParams: false,
      memo: '${action.toUpperCase()} $name',
      cancelLocation: '/names',
      completionLocation: '/names',
    );
  }
}

rust_names.ApiManagedName _managedName(String name) =>
    rust_names.ApiManagedName(
      name: name,
      paymentAddress: 'uregtest1destination',
      phase: 'active',
      commitment: Uint8List.fromList([1, 2, 3]),
      commitWindowStart: BigInt.zero,
      commitWindowEnd: BigInt.zero,
      commitBlocksUntil: BigInt.zero,
      commitWindowOpen: false,
      revealWindowStart: BigInt.zero,
      revealWindowEnd: BigInt.zero,
      revealBlocksUntil: BigInt.zero,
      revealWindowOpen: false,
      refreshWindowStart: BigInt.from(100),
      refreshWindowEnd: BigInt.from(104),
      refreshBlocksUntil: BigInt.zero,
      refreshWindowOpen: true,
    );

class _RecoveryManagedNamesNotifier extends ManagedNamesNotifier {
  static List<rust_names.ApiManagedName> items = const [];
  static String? recovered;

  @override
  Future<List<rust_names.ApiManagedName>> build() async => items;

  @override
  Future<String?> recover(String name) async {
    recovered = name;
    return null;
  }
}

class _CooldownRegistrationNotifier extends NamesRegistrationNotifier {
  static int prepareCalls = 0;

  @override
  NamesRegistrationState build() => NamesRegistrationState(
    bondStatus: rust_names.ApiNamesBondStatus(
      state: 'needs_preparation',
      requiredZatoshi: BigInt.from(kNamesBondZatoshis),
      exactNoteCount: 0,
      spendableIronwoodZatoshi: BigInt.from(kNamesBondZatoshis),
    ),
    draftName: 'hodl',
    draftPaymentAddress: 'uregtest1destination',
    draftPhase: 'cooldown',
  );

  @override
  Future<rust_names.ApiNamesBondStatus?> refreshBondStatus() async =>
      state.bondStatus;

  @override
  Future<void> refreshDraftPhase() async {}

  @override
  Future<String?> prepareDraft({
    required String name,
    required String paymentAddress,
  }) async {
    prepareCalls += 1;
    state = NamesRegistrationState(
      bondStatus: state.bondStatus,
      error:
          'That name is in protocol cooldown and cannot be registered until height 1201.',
    );
    return null;
  }
}

class _ActiveAccountNotifier extends AccountNotifier {
  @override
  FutureOr<AccountState> build() => const AccountState(
    accounts: [AccountInfo(uuid: 'software-account', name: 'Wallet', order: 0)],
    activeAccountUuid: 'software-account',
    activeAddress: 'uregtest1destination',
  );
}

class _EmptyAccountNotifier extends AccountNotifier {
  @override
  FutureOr<AccountState> build() => const AccountState();
}

void main() {
  test('REVEAL proposal requires an unlocked account', () async {
    final container = ProviderContainer(
      overrides: [accountProvider.overrideWith(_EmptyAccountNotifier.new)],
    );
    addTearDown(container.dispose);

    final notifier = container.read(managedNamesProvider.notifier);
    expect(await notifier.beginReveal('alice'), isNull);
    expect(notifier.lastRevealError, 'Unlock your wallet first.');
  });

  testWidgets('status failures are not presented as a locked wallet', (
    tester,
  ) async {
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          namesDeploymentProfileProvider.overrideWithValue(
            kLocalRegtestNamesDeploymentProfile,
          ),
          namesStatusProvider.overrideWith(_FailingNamesStatusNotifier.new),
        ],
        child: MaterialApp(
          home: AppTheme(
            data: AppThemeData.light,
            child: const Scaffold(body: NamesView(showDesktopChrome: false)),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    expect(find.byKey(const ValueKey('names_status_error')), findsOneWidget);
    expect(find.text('Names unavailable'), findsOneWidget);
    expect(find.text('Wallet locked'), findsNothing);
    expect(
      find.byKey(const ValueKey('names_status_retry_button')),
      findsOneWidget,
    );
  });

  testWidgets('uninitialized state has no legacy bootstrap UX', (tester) async {
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          namesDeploymentProfileProvider.overrideWithValue(
            kLocalRegtestNamesDeploymentProfile,
          ),
          namesStatusProvider.overrideWith(
            _UninitializedNamesStatusNotifier.new,
          ),
          managedNamesProvider.overrideWith(_RecoveryManagedNamesNotifier.new),
        ],
        child: const MaterialApp(
          home: AppTheme(
            data: AppThemeData.light,
            child: Scaffold(body: NamesView(showDesktopChrome: false)),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    expect(find.byKey(const ValueKey('names_state_ready')), findsOneWidget);
    expect(find.text('Bootstrap Names'), findsNothing);
    expect(find.textContaining('bootstrap'), findsNothing);
    expect(
      find.byKey(const ValueKey('names_registration_name_field')),
      findsOneWidget,
    );
    expect(find.byKey(const ValueKey('names_recovery_button')), findsOneWidget);
  });

  testWidgets('recover name is explicit and remains available with no names', (
    tester,
  ) async {
    _RecoveryManagedNamesNotifier.items = const [];
    _RecoveryManagedNamesNotifier.recovered = null;
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          namesDeploymentProfileProvider.overrideWithValue(
            kLocalRegtestNamesDeploymentProfile,
          ),
          namesStatusProvider.overrideWith(() => _ReadyNamesStatusNotifier()),
          managedNamesProvider.overrideWith(_RecoveryManagedNamesNotifier.new),
        ],
        child: const MaterialApp(
          home: AppTheme(
            data: AppThemeData.light,
            child: Scaffold(body: NamesView(showDesktopChrome: false)),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    expect(find.byKey(const ValueKey('names_recovery_button')), findsOneWidget);
    expect(_RecoveryManagedNamesNotifier.recovered, isNull);

    final recoveryButton = find.byKey(const ValueKey('names_recovery_button'));
    await tester.ensureVisible(recoveryButton);
    await tester.tap(recoveryButton);
    await tester.pumpAndSettle();
    await tester.enterText(
      find.byKey(const ValueKey('names_recovery_field')),
      'hodl.zec',
    );
    await tester.tap(find.byKey(const ValueKey('names_recovery_confirm')));
    await tester.pumpAndSettle();

    expect(_RecoveryManagedNamesNotifier.recovered, 'hodl.zec');
  });

  testWidgets('recover name remains available alongside multiple names', (
    tester,
  ) async {
    _RecoveryManagedNamesNotifier.items = [
      _managedName('first'),
      _managedName('second'),
    ];
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          namesDeploymentProfileProvider.overrideWithValue(
            kLocalRegtestNamesDeploymentProfile,
          ),
          namesStatusProvider.overrideWith(() => _ReadyNamesStatusNotifier()),
          managedNamesProvider.overrideWith(_RecoveryManagedNamesNotifier.new),
        ],
        child: const MaterialApp(
          home: AppTheme(
            data: AppThemeData.light,
            child: Scaffold(body: NamesView(showDesktopChrome: false)),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    expect(find.text('first.zec'), findsOneWidget);
    expect(find.text('second.zec'), findsOneWidget);
    expect(find.byKey(const ValueKey('names_recovery_button')), findsOneWidget);
  });

  testWidgets('cooldown registration click reports protocol state', (
    tester,
  ) async {
    _CooldownRegistrationNotifier.prepareCalls = 0;
    _RecoveryManagedNamesNotifier.items = [_managedName('hodl')];
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          accountProvider.overrideWith(_ActiveAccountNotifier.new),
          namesDeploymentProfileProvider.overrideWithValue(
            kLocalRegtestNamesDeploymentProfile,
          ),
          namesStatusProvider.overrideWith(() => _ReadyNamesStatusNotifier()),
          namesRegistrationProvider.overrideWith(
            _CooldownRegistrationNotifier.new,
          ),
          managedNamesProvider.overrideWith(_RecoveryManagedNamesNotifier.new),
        ],
        child: const MaterialApp(
          home: AppTheme(
            data: AppThemeData.light,
            child: Scaffold(body: NamesView(showDesktopChrome: false)),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    final button = find.byKey(const ValueKey('names_registration_button'));
    await tester.ensureVisible(button);
    await tester.tap(button);
    await tester.pumpAndSettle();

    expect(_CooldownRegistrationNotifier.prepareCalls, 1);
    expect(
      find.text(
        'That name is in protocol cooldown and cannot be registered until height 1201.',
      ),
      findsOneWidget,
    );
  });

  testWidgets('accepted REVEAL enters review without a scheduled height', (
    tester,
  ) async {
    SendReviewArgs? captured;
    final router = GoRouter(
      initialLocation: '/names',
      routes: [
        GoRoute(
          path: '/names',
          builder: (_, _) => const Scaffold(
            body: AppTheme(
              data: AppThemeData.light,
              child: NamesView(showDesktopChrome: false),
            ),
          ),
        ),
        GoRoute(
          path: '/send/review',
          builder: (_, state) {
            captured = state.extra as SendReviewArgs;
            return Text(
              '${captured!.address}|${captured!.feeZatoshi}|${captured!.memo}',
            );
          },
        ),
      ],
    );

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          namesDeploymentProfileProvider.overrideWithValue(
            kLocalRegtestNamesDeploymentProfile,
          ),
          namesStatusProvider.overrideWith(() => _ReadyNamesStatusNotifier()),
          managedNamesProvider.overrideWith(_ReviewManagedNamesNotifier.new),
        ],
        child: MaterialApp.router(routerConfig: router),
      ),
    );
    await tester.pumpAndSettle();

    final revealButton = find.byKey(
      const ValueKey('names_reveal_button_${_ReviewManagedNamesNotifier.name}'),
    );
    await tester.ensureVisible(revealButton);
    await tester.tap(revealButton);
    await tester.pumpAndSettle();

    expect(captured?.address, 'Coppice Names REVEAL');
    expect(captured?.feeZatoshi, BigInt.from(1234));
    expect(captured?.amountZatoshi, BigInt.from(kNamesBondZatoshis));
    expect(captured?.memo, 'Reveal reviewable');
    expect(
      find.text('Coppice Names REVEAL|1234|Reveal reviewable'),
      findsOneWidget,
    );
  });

  testWidgets('accepted COMMIT waits for its deterministic REVEAL window', (
    tester,
  ) async {
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          namesDeploymentProfileProvider.overrideWithValue(
            kLocalRegtestNamesDeploymentProfile,
          ),
          namesStatusProvider.overrideWith(() => _ReadyNamesStatusNotifier()),
          managedNamesProvider.overrideWith(_WaitingManagedNamesNotifier.new),
        ],
        child: MaterialApp(
          home: AppTheme(
            data: AppThemeData.light,
            child: const Scaffold(body: NamesView(showDesktopChrome: false)),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    expect(
      find.text('REVEAL window opens at height 120 (19 blocks)'),
      findsOneWidget,
    );
    expect(
      find.byKey(const ValueKey('names_reveal_button_waiting')),
      findsNothing,
    );
  });

  for (final action in ['update', 'renew', 'release']) {
    testWidgets('$action management enters shared transaction review', (
      tester,
    ) async {
      _ReviewManagementNotifier.capturedAction = null;
      _ReviewManagementNotifier.capturedAddress = null;
      SendReviewArgs? captured;
      final router = GoRouter(
        initialLocation: '/names',
        routes: [
          GoRoute(
            path: '/names',
            builder: (_, _) => const Scaffold(
              body: AppTheme(
                data: AppThemeData.light,
                child: NamesView(showDesktopChrome: false),
              ),
            ),
          ),
          GoRoute(
            path: '/send/review',
            builder: (_, state) {
              captured = state.extra as SendReviewArgs;
              return Text('review:${captured!.address}');
            },
          ),
        ],
      );
      await tester.binding.setSurfaceSize(const Size(1200, 900));
      addTearDown(() => tester.binding.setSurfaceSize(null));
      await tester.pumpWidget(
        ProviderScope(
          overrides: [
            namesDeploymentProfileProvider.overrideWithValue(
              kLocalRegtestNamesDeploymentProfile,
            ),
            namesStatusProvider.overrideWith(() => _ReadyNamesStatusNotifier()),
            managedNamesProvider.overrideWith(_ReviewManagementNotifier.new),
          ],
          child: MaterialApp.router(routerConfig: router),
        ),
      );
      await tester.pumpAndSettle();

      await tester.ensureVisible(find.byTooltip('Manage managed.zec'));
      await tester.tap(find.byTooltip('Manage managed.zec'));
      await tester.pumpAndSettle();
      await tester.tap(
        find.text(switch (action) {
          'update' => 'Update address',
          'renew' => 'Renew lease',
          _ => 'Release name',
        }),
      );
      await tester.pumpAndSettle();

      if (action == 'update') {
        await tester.enterText(
          find.widgetWithText(TextFormField, 'New payment address'),
          'uregtest1new',
        );
        await tester.tap(find.text('Update').last);
        await tester.pumpAndSettle();
      } else if (action == 'release') {
        await tester.tap(find.text('Release').last);
        await tester.pumpAndSettle();
      }

      expect(_ReviewManagementNotifier.capturedAction, action);
      expect(
        _ReviewManagementNotifier.capturedAddress,
        action == 'update' ? 'uregtest1new' : isNull,
      );
      expect(captured?.amountZatoshi, BigInt.from(kNamesBondZatoshis));
      expect(captured?.cancelLocation, '/names');
      expect(captured?.completionLocation, '/names');
      expect(
        find.text('review:Coppice Names ${action.toUpperCase()}'),
        findsOneWidget,
      );
    });
  }
}

class _ReadyNamesStatusNotifier extends NamesStatusNotifier {
  @override
  Future<rust_names.ApiNamesWalletStatus?> build() async =>
      rust_names.ApiNamesWalletStatus(
        state: 'ready',
        message: 'ready',
        configured: true,
        tipHeight: BigInt.from(100),
        namesActivationHeight: BigInt.from(1),
        oldestRewindHeight: BigInt.from(1),
      );
}
