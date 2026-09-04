import 'package:flutter_test/flutter_test.dart';
import 'package:zcash_wallet/src/features/names/services/zec_name_resolution.dart';
import 'package:zcash_wallet/src/rust/api/names.dart' as rust_names;

rust_names.ApiNamesResolution resolution(
  String status, {
  String? paymentAddress,
}) {
  return rust_names.ApiNamesResolution(
    status: status,
    paymentAddress: paymentAddress,
    leaseExpiry: BigInt.from(120),
    terminalHeight: null,
    producerTxid: null,
    producerHeight: null,
    producerTxIndex: null,
    producerActionIndex: null,
    tipHeight: BigInt.from(100),
    compactBlocksScanned: BigInt.zero,
  );
}

void main() {
  group('looksLikeZecName', () {
    test('accepts trimmed .zec input and rejects everything else', () {
      expect(looksLikeZecName(' alice.zec '), isTrue);
      expect(looksLikeZecName('ALICE.ZEC'), isTrue);
      expect(looksLikeZecName('.zec'), isFalse);
      expect(looksLikeZecName('u1abc'), isFalse);
      expect(looksLikeZecName(''), isFalse);
    });
  });

  group('zecNameLabelValidationError', () {
    test('accepts canonical labels and normalized presentation', () {
      expect(zecNameLabelValidationError('alice'), isNull);
      expect(zecNameLabelValidationError(' Alice-42 '), isNull);
      expect(zecNameLabelValidationError('a'), isNull);
      expect(zecNameLabelValidationError('a' * 63), isNull);
    });

    test('rejects suffixes, invalid characters, boundaries, and length', () {
      expect(zecNameLabelValidationError('alice.zec'), isNotNull);
      expect(zecNameLabelValidationError('alice_42'), isNotNull);
      expect(zecNameLabelValidationError('-alice'), isNotNull);
      expect(zecNameLabelValidationError('alice-'), isNotNull);
      expect(zecNameLabelValidationError('a' * 64), isNotNull);
      expect(zecNameLabelValidationError('ليس'), isNotNull);
    });
  });

  group('changedZecNameRecipientMessage', () {
    test('accepts a fresh result when the payment address is unchanged', () {
      expect(
        changedZecNameRecipientMessage(
          name: 'alice.zec',
          previousAddress: 'uregtest1same',
          current: ZecNameResolution(
            name: 'alice.zec',
            paymentAddress: 'uregtest1same',
            lifecycleStatus: 'active',
            leaseExpiryHeight: BigInt.from(200),
            tipHeight: BigInt.from(101),
          ),
        ),
        isNull,
      );
    });

    test('requires another review when the payment address changed', () {
      expect(
        changedZecNameRecipientMessage(
          name: 'alice.zec',
          previousAddress: 'uregtest1old',
          current: ZecNameResolution(
            name: 'alice.zec',
            paymentAddress: 'uregtest1new',
            lifecycleStatus: 'active',
            leaseExpiryHeight: BigInt.from(200),
            tipHeight: BigInt.from(102),
          ),
        ),
        contains('different address'),
      );
    });
  });

  group('zecNameResolutionFromApi', () {
    test('active yields the payment address and lease metadata', () {
      final result = zecNameResolutionFromApi(
        'alice.zec',
        resolution('active', paymentAddress: ' u1payment '),
      );
      expect(result.name, 'alice.zec');
      expect(result.paymentAddress, 'u1payment');
      expect(result.lifecycleStatus, 'active');
      expect(result.leaseExpiryHeight, BigInt.from(120));
      expect(result.tipHeight, BigInt.from(100));
    });

    test('active without a payment address is refused', () {
      expect(
        () => zecNameResolutionFromApi(
          'alice.zec',
          resolution('active', paymentAddress: null),
        ),
        throwsA(
          isA<ZecNameResolutionException>().having(
            (e) => e.status,
            'status',
            'active',
          ),
        ),
      );
    });

    test('every non-active lifecycle is refused with user-facing text', () {
      const statuses = ['cooldown', 'missing', 'surprise'];
      for (final status in statuses) {
        expect(
          () => zecNameResolutionFromApi('alice.zec', resolution(status)),
          throwsA(isA<ZecNameResolutionException>()),
          reason: 'status $status should throw',
        );
      }
    });

    test('cooldown does not claim former-owner priority', () {
      expect(
        () => zecNameResolutionFromApi('alice.zec', resolution('cooldown')),
        throwsA(
          isA<ZecNameResolutionException>()
              .having(
                (error) => error.message,
                'message',
                contains('cannot be registered by anyone'),
              )
              .having(
                (error) => error.message,
                'message',
                isNot(contains('previous owner')),
              ),
        ),
      );
    });

    test('refusals carry the lifecycle status for programmatic use', () {
      try {
        zecNameResolutionFromApi('alice.zec', resolution('missing'));
        fail('expected ZecNameResolutionException');
      } on ZecNameResolutionException catch (error) {
        expect(error.status, 'missing');
        expect(error.message, contains('not registered'));
      }
    });
  });
}
