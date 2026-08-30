import 'package:flutter/widgets.dart';

import '../names_screen.dart' show NamesView;

/// Mobile Coppice/Names tab root. The tab shell owns the bottom bar; the
/// shared [NamesView] provides the content.
class MobileNamesScreen extends StatelessWidget {
  const MobileNamesScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return const NamesView(showDesktopChrome: false);
  }
}
