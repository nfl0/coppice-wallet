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

class _ReviewManagedNamesNotifier extends ManagedNamesNotifier {
  static const name = 'reviewable';

  @override
  Future<List<rust_names.ApiManagedName>> build() async => [
    rust_names.ApiManagedName(
      name: name,
      phase: 'commit_accepted',
      commitment: Uint8List.fromList([1, 2, 3]),
      nextRevealHeight: BigInt.from(101),
      revealBlocksUntil: BigInt.zero,
      revealReady: true,
    ),
  ];

  @override
  Future<SendReviewArgs?> beginReveal(String name) async => SendReviewArgs(
    proposalId: BigInt.from(42),
    sendFlowId: 'names-review-flow',
    proposalAccountUuid: 'software-account',
    address: 'Coppice Names REVEAL',
    addressType: 'unified',
    amountZatoshi: BigInt.one,
    feeZatoshi: BigInt.from(1234),
    needsSaplingParams: false,
    memo: 'Reveal ${name.trim().toLowerCase()}',
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

  testWidgets('eligible REVEAL enters the ordinary send review route', (
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
      const ValueKey(
        'names_reveal_button_${_ReviewManagedNamesNotifier.name}',
      ),
    );
    await tester.ensureVisible(revealButton);
    await tester.tap(revealButton);
    await tester.pumpAndSettle();

    expect(captured?.address, 'Coppice Names REVEAL');
    expect(captured?.feeZatoshi, BigInt.from(1234));
    expect(captured?.amountZatoshi, BigInt.one);
    expect(captured?.memo, 'Reveal reviewable');
    expect(
      find.text('Coppice Names REVEAL|1234|Reveal reviewable'),
      findsOneWidget,
    );
  });
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
