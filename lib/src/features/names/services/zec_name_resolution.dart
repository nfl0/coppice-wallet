import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../rust/api/names.dart' as rust_names;

/// Suffix that marks user input as a Coppice/Names name instead of an
/// address. Addresses cannot contain `.`, so the distinction is unambiguous.
const kZecNameSuffix = '.zec';

/// Debounce applied before resolving a typed `.zec` name. The first lookup may
/// replay from activation; later lookups advance the cached authenticated
/// resolver, so keystrokes must not each start resolver work.
const kZecNameResolveDebounce = Duration(milliseconds: 600);

/// A successfully resolved `.zec` name, carrying the payment address the
/// send flow should use in place of the typed name.
class ZecNameResolution {
  const ZecNameResolution({
    required this.name,
    required this.paymentAddress,
    required this.lifecycleStatus,
    required this.leaseExpiryHeight,
    required this.tipHeight,
  });

  /// Normalized (trim + lowercase) name the input resolved to.
  final String name;
  final String paymentAddress;
  final String lifecycleStatus;
  final BigInt? leaseExpiryHeight;
  final BigInt tipHeight;
}

/// Thrown with a user-facing message when a `.zec` name cannot be used as a
/// payment destination.
class ZecNameResolutionException implements Exception {
  const ZecNameResolutionException(this.message, {this.status});

  final String message;

  /// The lifecycle status the resolver returned, when it did.
  final String? status;

  @override
  String toString() => message;
}

typedef ZecNameResolver =
    Future<ZecNameResolution> Function(
      String input, {
      required String dbPath,
      required String lightwalletdUrl,
      required String network,
    });

/// Injectable resolver boundary used by the Names and send UIs. Production
/// delegates to Rust's exact authenticated resolver; widget tests can replace it without
/// initializing the Rust bridge or a network.
final zecNameResolverProvider = Provider<ZecNameResolver>(
  (ref) => resolveZecNameInput,
);

/// Returns user-facing copy when a fresh result no longer matches the address
/// the user originally reviewed. The caller must stop and require another
/// explicit review instead of silently changing the payment recipient.
String? changedZecNameRecipientMessage({
  required String name,
  required String previousAddress,
  required ZecNameResolution current,
}) {
  if (current.paymentAddress == previousAddress) return null;
  return '`$name` now resolves to a different address. Review the new '
      'recipient before continuing.';
}

/// True when [input] should be handled as a `.zec` name rather than an
/// address. Presentation syntax is normalized (trim + lowercase) by the
/// resolvers below; anything ending in `.zec` is a name, and the Rust
/// resolver validates the name's syntax authoritatively.
bool looksLikeZecName(String input) {
  final trimmed = input.trim().toLowerCase();
  return trimmed.length > kZecNameSuffix.length &&
      trimmed.endsWith(kZecNameSuffix);
}

final RegExp _zecNameLabelPattern = RegExp(r'^[a-z0-9]([a-z0-9-]*[a-z0-9])?$');

/// Returns user-facing copy when [input] is not a valid bare registration
/// label, or `null` when it is. Canonical labels are 1-63 bytes of
/// `[a-z0-9-]` with no leading or trailing hyphen (see
/// `coppice_names::protocol::Name`). The Rust host enforces the
/// same rules authoritatively; this mirrors them so users get immediate,
/// actionable feedback before any bond preparation starts.
String? zecNameLabelValidationError(String input) {
  final label = input.trim().toLowerCase();
  if (label.isEmpty) return 'Enter a name label.';
  if (label.contains('.')) {
    return 'Enter the label only; .zec is added automatically.';
  }
  if (label.length > 63) {
    return 'Name labels can be at most 63 characters.';
  }
  if (!_zecNameLabelPattern.hasMatch(label)) {
    return 'Use lowercase letters, digits, and hyphens only, with no '
        'leading or trailing hyphen.';
  }
  return null;
}

/// Normalizes `.zec` presentation (trim + lowercase) and resolves the name
/// through the wallet's authenticated exact resolver against the active endpoint.
///
/// Only an `active` lease passes — the payment flow must not send to a
/// cooldown, claimable, or missing name. Every other
/// outcome throws [ZecNameResolutionException] with a user-facing message.
Future<ZecNameResolution> resolveZecNameInput(
  String input, {
  required String dbPath,
  required String lightwalletdUrl,
  required String network,
}) async {
  final name = input.trim().toLowerCase();
  if (!looksLikeZecName(name)) {
    throw const ZecNameResolutionException('Enter a name ending in .zec');
  }
  final rust_names.ApiNamesResolution resolution;
  try {
    resolution = await rust_names.resolveName(
      dbPath: dbPath,
      lightwalletdUrl: lightwalletdUrl,
      network: network,
      name: name,
    );
  } catch (error) {
    throw ZecNameResolutionException(friendlyZecNameResolutionError(error));
  }
  return zecNameResolutionFromApi(name, resolution);
}

/// Maps a raw resolver result into a [ZecNameResolution], honoring the
/// returned lifecycle: only `active` yields a usable payment address.
ZecNameResolution zecNameResolutionFromApi(
  String name,
  rust_names.ApiNamesResolution resolution,
) {
  switch (resolution.status) {
    case 'active':
      final address = resolution.paymentAddress?.trim() ?? '';
      if (address.isEmpty) {
        throw ZecNameResolutionException(
          '`$name` has no payment address in its record',
          status: resolution.status,
        );
      }
      return ZecNameResolution(
        name: name,
        paymentAddress: address,
        lifecycleStatus: resolution.status,
        leaseExpiryHeight: resolution.leaseExpiry,
        tipHeight: resolution.tipHeight,
      );
    case 'cooldown':
      throw ZecNameResolutionException(
        '`$name` is in cooldown and cannot be registered by anyone yet',
        status: resolution.status,
      );
    case 'claimable':
      throw ZecNameResolutionException(
        '`$name` is available to register',
        status: resolution.status,
      );
    case 'missing':
      throw ZecNameResolutionException(
        '`$name` is not registered',
        status: resolution.status,
      );
    default:
      throw ZecNameResolutionException(
        '`$name` could not be resolved',
        status: resolution.status,
      );
  }
}

/// User-facing text for transport-level resolver failures (endpoint down,
/// Names not configured yet, and so on).
String friendlyZecNameResolutionError(Object error) {
  final text = error.toString().toLowerCase();
  if (text.contains('not configured') || text.contains('disabled')) {
    return 'Coppice Names is not set up for this wallet yet.';
  }
  if (text.contains('bootstrap')) {
    return 'Coppice Names is preparing authenticated state. Try again shortly.';
  }
  if (text.contains('connect') || text.contains('endpoint')) {
    return 'Name resolution failed. Check your connection and try again.';
  }
  return 'Name resolution failed. Try again in a moment.';
}
