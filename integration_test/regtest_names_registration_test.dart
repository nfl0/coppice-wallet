import 'dart:convert';
import 'dart:io';

import 'package:flutter/widgets.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:zcash_wallet/app.dart';
import 'package:zcash_wallet/src/core/config/network_config.dart';
import 'package:zcash_wallet/src/features/names/providers/names_provider.dart';
import 'package:zcash_wallet/src/providers/sync_provider.dart';

import 'support/desktop_onboarding_flow.dart';
import 'support/desktop_regtest_flow.dart';

const _faucetMnemonic =
    'abandon abandon abandon abandon abandon abandon abandon abandon abandon '
    'abandon abandon abandon abandon abandon abandon abandon abandon abandon '
    'abandon abandon abandon abandon abandon art';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  setUpAll(initializeZcashWalletRuntime);

  testWidgets(
    'registers a name through the desktop UI with manual REVEAL',
    (tester) async {
      final names = List<String>.generate(
        4,
        (index) => 'walletgui${DateTime.now().millisecondsSinceEpoch}$index',
      );
      if (kZcashDefaultNetworkName != ZcashNetwork.regtest.name) {
        fail('This test must run with ZCASH_DEFAULT_NETWORK=regtest.');
      }
      addTearDown(cleanupDesktopRegtestWallet);
      await cleanupDesktopRegtestWallet();

      await tester.pumpWidget(await buildBootstrappedZcashWalletApp());
      await tapAppButton(
        tester,
        const ValueKey('welcome_import_wallet_button'),
      );
      await enterAppText(
        tester,
        const ValueKey('import_mnemonic_first_word_field'),
        _faucetMnemonic,
      );
      await tapAppButton(tester, const ValueKey('import_secret_submit_button'));
      await tapAppButton(tester, const ValueKey('import_birthday_skip_button'));
      await tapAppButton(
        tester,
        const ValueKey('unknown_birthday_confirm_button'),
      );
      await enterAppText(
        tester,
        const ValueKey('set_password_password_field'),
        desktopRegtestPassword,
      );
      await enterAppText(
        tester,
        const ValueKey('set_password_confirm_field'),
        desktopRegtestPassword,
      );
      await tapAppButton(tester, const ValueKey('set_password_submit_button'));
      await finishDesktopAccountCustomisation(tester);

      await pumpUntil(
        tester,
        () => tester.any(
          find.byKey(const ValueKey('home_desktop_balance_amount_text')),
        ),
        description: 'desktop wallet home',
        timeout: const Duration(minutes: 2),
      );
      await tapAppWidget(tester, const ValueKey('sidebar_names_button'));
      await _ensureNamesReady(tester);

      for (var index = 0; index < names.length; index++) {
        final name = names[index];
        await enterAppText(
          tester,
          const ValueKey('names_registration_name_field'),
          name,
        );
        await pumpUntil(tester, () {
          final field = find.descendant(
            of: find.byKey(const ValueKey('names_registration_address_field')),
            matching: find.byType(EditableText),
          );
          return tester.any(field) &&
              tester.widget<EditableText>(field).controller.text.isNotEmpty;
        }, description: 'prefilled registration payment address');

        const registrationButton = ValueKey('names_registration_button');
        await pumpUntil(
          tester,
          () => textForKey(tester, registrationButton) != null,
          description: 'registration action for $name',
        );
        await tapAppButton(tester, registrationButton);
        final preparedBond = await _confirmSend(
          tester,
          description: 'Names registration first transaction',
        );
        if (preparedBond) {
          final bondHeight = await _mine(3);
          await _syncToHeight(tester, bondHeight);
          await tapAppWidget(tester, const ValueKey('sidebar_names_button'));
          await pumpUntil(
            tester,
            () => find.text('Continue registration').evaluate().isNotEmpty,
            description: 'confirmed exact bond to become reserved',
            timeout: const Duration(minutes: 3),
          );
          await tapAppButton(
            tester,
            ValueKey('names_resume_registration_$name'),
          );
          await _confirmSend(tester, description: 'Names COMMIT for $name');
        }

        // Mine one block at a time and make the wallet finish applying that
        // exact tip before checking the one-block REVEAL window.
        for (var block = 0; block < 20; block++) {
          final minedHeight = await _mine(1);
          await _syncToHeight(tester, minedHeight);
          if (block == 0) {
            await tapAppWidget(tester, const ValueKey('sidebar_names_button'));
          }
          if (find.text('Reveal now').evaluate().isNotEmpty) break;
        }
        await pumpUntil(
          tester,
          () => find.text('Reveal now').evaluate().isNotEmpty,
          description: 'manual REVEAL action for $name',
          timeout: const Duration(minutes: 1),
        );
        await tapAppButton(tester, ValueKey('names_reveal_button_$name'));
        await _confirmSend(tester, description: 'Names REVEAL for $name');

        final revealHeight = await _mine(1);
        await _syncToHeight(tester, revealHeight);
        await tapAppWidget(tester, const ValueKey('sidebar_names_button'));
        final currentRow = find.byKey(ValueKey('managed_name_row_$name'));
        await pumpUntil(
          tester,
          () =>
              tester.any(currentRow) &&
              tester.any(
                find.descendant(of: currentRow, matching: find.text('Active')),
              ),
          description: '$name to become active',
          timeout: const Duration(minutes: 3),
        );
        for (final earlierName in names.take(index)) {
          final row = find.byKey(ValueKey('managed_name_row_$earlierName'));
          expect(row, findsOneWidget);
          // The intentionally short 32-block regtest lease can naturally
          // expire an early name while later name-derived anchors are mined.
          // What must never happen is spending its managed state note through
          // ordinary wallet note selection.
          expect(
            find.descendant(
              of: row,
              matching: find.text('State note spent outside Names'),
            ),
            findsNothing,
          );
        }
      }

      // The send flow accepts the registered name as the recipient and pays
      // the resolved payment address.
      await _sendToName(tester, '${names.last}.zec');
    },
    timeout: const Timeout(Duration(minutes: 45)),
  );
}

Future<void> _syncToHeight(WidgetTester tester, int targetHeight) async {
  final app = tester.element(find.byType(WidgetsApp).first);
  final container = ProviderScope.containerOf(app);
  final deadline = DateTime.now().add(const Duration(minutes: 2));
  while (DateTime.now().isBefore(deadline)) {
    final completedBefore = container
        .read(syncProvider)
        .value
        ?.lastSyncCompletedAt;
    await container.read(syncProvider.notifier).restartSync();
    while (DateTime.now().isBefore(deadline)) {
      await tester.pump(const Duration(milliseconds: 500));
      final sync = container.read(syncProvider).value;
      if (sync != null &&
          !sync.isSyncing &&
          sync.isSyncComplete &&
          sync.lastSyncCompletedAt != null &&
          sync.lastSyncCompletedAt != completedBefore &&
          sync.isSyncedToTip &&
          sync.chainTipHeight >= targetHeight &&
          sync.scannedHeight >= targetHeight) {
        await container.read(namesStatusProvider.notifier).refresh();
        await container.read(managedNamesProvider.notifier).refresh();
        await tester.pump();
        return;
      }
      // A completed run below the exact Zakura target means Zaino had not
      // indexed the new block yet. Back off and start one fresh run; never
      // cancel a sync that is still making progress.
      if (sync != null && !sync.isSyncing && sync.isSyncComplete) {
        break;
      }
    }
    await tester.pump(const Duration(milliseconds: 500));
  }
  final sync = container.read(syncProvider).value;
  fail(
    'wallet did not apply mined height $targetHeight '
    '(server ${sync?.chainTipHeight}, scanned ${sync?.scannedHeight})',
  );
}

/// Sends ZEC to a `.zec` name through the desktop send screen. Tapping the
/// review button implicitly waits for the debounced name resolution: the
/// button stays disabled until the resolved address validates.
Future<void> _sendToName(WidgetTester tester, String name) async {
  await tapAppWidget(tester, const ValueKey('sidebar_home_button'));
  await tapAppWidget(tester, const ValueKey('home_desktop_send_button'));
  await pumpUntil(
    tester,
    () => tester.any(find.byKey(const ValueKey('send_address_field'))),
    description: 'send compose screen',
  );
  await enterAppText(tester, const ValueKey('send_address_field'), name);
  await enterAppText(tester, const ValueKey('send_amount_field'), '0.01');
  await _confirmSend(tester, description: 'payment to $name');
}

Future<void> _ensureNamesReady(WidgetTester tester) async {
  await pumpUntil(
    tester,
    () =>
        tester.any(find.byKey(const ValueKey('names_state_ready'))) ||
        tester.any(find.byKey(const ValueKey('names_bootstrap_button'))) ||
        tester.any(find.byKey(const ValueKey('names_configure_button'))),
    description: 'Names deployment state',
    timeout: const Duration(minutes: 3),
  );
  if (tester.any(find.byKey(const ValueKey('names_configure_button')))) {
    await tapAppButton(tester, const ValueKey('names_configure_button'));
    await pumpUntil(
      tester,
      () =>
          tester.any(find.byKey(const ValueKey('names_state_ready'))) ||
          tester.any(find.byKey(const ValueKey('names_bootstrap_button'))),
      description: 'configured Names deployment',
      timeout: const Duration(minutes: 2),
    );
  }
  if (tester.any(find.byKey(const ValueKey('names_bootstrap_button')))) {
    await tapAppButton(tester, const ValueKey('names_bootstrap_button'));
  }
  await pumpUntil(
    tester,
    () => tester.any(find.byKey(const ValueKey('names_state_ready'))),
    description: 'Names bootstrap',
    timeout: const Duration(minutes: 4),
  );
}

/// Confirms the current reviewed send and returns whether it used the ordinary
/// send proposal screen (the 1 ZEC bond preparation path) rather than a direct
/// Names capability review.
Future<bool> _confirmSend(
  WidgetTester tester, {
  required String description,
}) async {
  await pumpUntil(
    tester,
    () =>
        tester.any(find.byKey(const ValueKey('send_review_button'))) ||
        tester.any(find.byKey(const ValueKey('send_confirm_button'))),
    description: '$description review',
    timeout: const Duration(minutes: 2),
  );
  final usedOrdinaryProposal = tester.any(
    find.byKey(const ValueKey('send_review_button')),
  );
  if (usedOrdinaryProposal) {
    await tapAppButton(tester, const ValueKey('send_review_button'));
  }
  await tapAppButton(tester, const ValueKey('send_confirm_button'));
  await pumpUntil(
    tester,
    () => tester.any(find.byKey(const ValueKey('send_status_completed'))),
    description: '$description transaction broadcast',
    timeout: const Duration(minutes: 4),
  );
  return usedOrdinaryProposal;
}

Future<int> _mine(int count) async {
  await _rpc('generate', [count]);
  final height = await _rpc('getblockcount', const []);
  return height as int;
}

Future<Object?> _rpc(String method, List<Object?> params) async {
  final client = HttpClient();
  try {
    final request = await client.postUrl(Uri.parse('http://127.0.0.1:18232'));
    request.headers.contentType = ContentType.json;
    request.headers.set(
      HttpHeaders.authorizationHeader,
      'Basic ${base64.encode(utf8.encode('xxxxxx:xxxxxx'))}',
    );
    request.write(
      jsonEncode({
        'jsonrpc': '1.0',
        'id': 'names-ui-test',
        'method': method,
        'params': params,
      }),
    );
    final response = await request.close();
    final body = await utf8.decoder.bind(response).join();
    if (response.statusCode != HttpStatus.ok) {
      throw StateError('$method returned ${response.statusCode}: $body');
    }
    final decoded = jsonDecode(body) as Map<String, dynamic>;
    if (decoded['error'] != null) {
      throw StateError('$method failed: ${decoded['error']}');
    }
    return decoded['result'];
  } finally {
    client.close(force: true);
  }
}
