import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:zcash_wallet/src/core/theme/app_theme.dart';
import 'package:zcash_wallet/src/features/names/models/names_deployment.dart';
import 'package:zcash_wallet/src/features/names/providers/names_provider.dart';
import 'package:zcash_wallet/src/features/names/screens/names_screen.dart';
import 'package:zcash_wallet/src/rust/api/names.dart' as rust_names;

class _FailingNamesStatusNotifier extends NamesStatusNotifier {
  @override
  Future<rust_names.ApiNamesWalletStatus?> build() async {
    throw StateError('sidecar read failed');
  }
}

void main() {
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
}
