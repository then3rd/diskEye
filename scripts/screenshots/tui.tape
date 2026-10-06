# TUI screenshots for the README, rendered by VHS (https://github.com/charmbracelet/vhs).
# Run from the repo root via `just screenshots`, which writes the demo
# snapshots and sets DEMO_SNAPSHOT, DISKEYE_NOW, TZ and XDG_STATE_HOME.
Output target/demo-state/tui.gif
Set Shell bash
Set FontSize 14
Set Width 1400
Set Height 820
Set Padding 16
Set Theme "GitHub Dark"

Hide
Type `target/debug/diskeye tui "$DEMO_SNAPSHOT"` Enter
Sleep 1.5s
Show
Screenshot docs/screenshots/tui-physical.png
Sleep 300ms

Type "2"
Sleep 300ms
Screenshot docs/screenshots/tui-files.png
Sleep 300ms

Type "t"
Sleep 300ms
Screenshot docs/screenshots/tui-treemap.png
Sleep 300ms
Escape

Type "3"
Sleep 300ms
Screenshot docs/screenshots/tui-workloads.png
Sleep 300ms

Type "4"
Type "   "
Sleep 300ms
Screenshot docs/screenshots/tui-reclaim.png
Sleep 300ms

Type "5"
Sleep 300ms
Screenshot docs/screenshots/tui-reconcile.png
Sleep 300ms

Type "6"
Sleep 1.5s
Screenshot docs/screenshots/tui-diff.png
Sleep 300ms

Hide
Type "q"
