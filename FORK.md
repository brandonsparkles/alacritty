# Alacritty Fork — macOS Feature Additions

This branch (`feature/named-tabs-and-swipe`) is a fork of [upstream
Alacritty v0.17.0](https://github.com/alacritty/alacritty) with macOS-specific
quality-of-life additions, layered on top of the existing native NSWindow
tab support.

All features are gated on `target_os = "macos"`; the non-macOS build is
byte-for-byte the same as upstream apart from a few enum variants that
non-macOS code never reaches.

## Added features

| Feature | Trigger / behavior |
|---|---|
| **Two-finger swipe between tabs** | Horizontal trackpad pan accumulating ≥ 50 px (within 25° of horizontal) fires `SelectNextTab` / `SelectPreviousTab`. Swipe right → previous, swipe left → next (Apple swipe-between-pages convention). |
| **Inline tab rename** | `Cmd+Shift+R` opens a live prompt — keystrokes update the NSWindowTab title in place. `Enter` commits, `Esc` cancels, `Ctrl+W` deletes a word, `Ctrl+U` clears, `Backspace` deletes a character. |
| **Reset tab title** | `Cmd+Shift+Opt+R` clears any user override, reverting to the auto-derived window title. |
| **Activity indicator** | A `⠿ ` prefix is added to the tab label whenever the shell has any direct child process (vim, codex, claude, copilot, sleep, etc.). Detection: `proc_listpids(PROC_PPID_ONLY, shell_pid)` every 500 ms — works regardless of how the subprocess manages its process group. |
| **Needs-attention indicator** | A `🔵 ` prefix is added when the terminal bell rings while the tab is unfocused. Cleared on focus. |
| **Close confirmation** | `Cmd+W`, `Cmd+Q`, and red-button close raise a native `NSAlert` ("Close" / "Cancel") when a foreground subprocess is running. "Close" is the default (gets the Return key); "Cancel" gets Escape. |
| **Double-Escape clears input line** | Pressing Escape twice within 400 ms emits `Ctrl-A` + `Ctrl-K` (`\x01\x0b`) to the PTY after the normal Escape, clearing the current readline / TUI prompt input. Also exposed as bindable `Action::ClearInputLine`. |
| **Session restoration** | Windows reopen on next launch with their cwd, tab group, tab title, size, and screen position preserved. Persisted to `~/Library/Application Support/org.alacritty/session.json`. Hold Shift at launch to opt out. Skipped when the CLI specifies `-e`, `--working-directory`, or `--title`. |

## Keybindings reference

All added bindings live in `alacritty/src/config/bindings.rs` under the
macOS `platform_key_bindings()` function and can be overridden in your
`alacritty.toml` like any upstream binding:

| Key | Action |
|---|---|
| `Cmd+Shift+R` | `PromptRenameTab` |
| `Cmd+Shift+Opt+R` | `ResetTabTitle` |
| `Enter` (in rename mode) | `TabRename::Confirm` |
| `Esc` (in rename mode) | `TabRename::Cancel` |
| `Ctrl+W` (in rename mode) | `TabRename::DeleteWord` |
| `Ctrl+U` (in rename mode) | `TabRename::Clear` |
| `Backspace` (in rename mode) | `TabRename::DeleteChar` |
| `Ctrl+C` (in rename mode) | `TabRename::Cancel` |

To rebind, e.g., the inline rename to `Cmd+R` instead:

```toml
[[keyboard.bindings]]
key = "R"
mods = "Command"
action = "PromptRenameTab"
```

To bind `ClearInputLine` to `Cmd+Backspace` (so you don't have to double-tap Escape):

```toml
[[keyboard.bindings]]
key = "Back"
mods = "Command"
action = "ClearInputLine"
```

## Configuration interactions

- `window.dynamic_title = false` is respected: the static window title is
  used as the base for the activity prefix (so you see `⠿ Alacritty`
  instead of `⠿ <shell-derived-title>`). Cmd+Shift+R rename overrides
  still work and compose with the prefix.
- `window.decorations = "None"` disables all native tab features (upstream
  behavior — tabs require a titlebar). Cmd+W/Q confirmation still works.
- `mouse.bindings` for `WheelUp` / `WheelDown` (new in upstream 0.17.0)
  can be used alongside the gesture detection if you want per-tick
  wheel-to-tab in addition to the gesture.

## Troubleshooting

**Features stopped working after a Homebrew upgrade.** `brew upgrade
alacritty` overwrites `/Applications/Alacritty.app/Contents/MacOS/alacritty`.
Run your `/update` slash command to rebuild and reinstall the fork
automatically (it tracks `.fork-sha` in the bundle and rebuilds whenever
that marker doesn't match `git HEAD`). Manual fallback:

```bash
cd ~/Desktop/Projects/alacritty
cargo build --release -p alacritty
cp target/release/alacritty /Applications/Alacritty.app/Contents/MacOS/alacritty
codesign --force --deep --sign - /Applications/Alacritty.app
echo "$(git rev-parse HEAD)" > /Applications/Alacritty.app/Contents/MacOS/.fork-sha
```

**Activity indicator never appears.** Check that `proc_listpids` sees the
child — `pgrep -P $(pgrep -fl 'login -fp brandonlind' | head -1 | awk '{print $1}')`
should list at least the subprocess you ran. If it does, but the indicator
is still missing, the 500 ms polling tick may have been unscheduled — relaunch
to retrigger.

**Session doesn't restore.** Verify the saved-state file exists:

```bash
ls -la "$HOME/Library/Saved Application State/org.alacritty.savedState/"
```

If `session.json` is missing, the periodic save hasn't fired yet (give it
~5 seconds after launch). If it's present but old, the snapshot writer
may have been blocked by I/O — turn on event logging in the config:

```toml
[debug]
print_events = true
log_level = "Trace"
```

then watch the log for `TabActivityTick` / `SessionSaveTick` user events
to confirm the timers are firing.

**Drag-out doesn't work.** It's intentionally disabled — see the table
above. Click-in-existing-selection now falls back to clearing and
starting a new selection.

**Cmd+Q closes without prompting.** Confirmation only fires when the
shell has a foreground subprocess. At an idle shell prompt, Cmd+Q quits
immediately by design.

## Updating

### Pulling upstream into the fork

```bash
cd ~/Desktop/Projects/alacritty
git fetch origin
git checkout feature/named-tabs-and-swipe
git rebase origin/master   # or merge — your preference
# resolve conflicts if any
cargo build --release -p alacritty
# /update or the manual install commands above
```

### After upstream releases a new version

The `/update` skill handles the rebuild + reinstall automatically. After
pulling upstream, you may need to re-verify that the fork's binding tables
still resolve (`platform_key_bindings()` may grow new entries upstream).

## Files added/touched

```
alacritty/src/session.rs          (NEW)  — session persistence
alacritty/src/display/drag_source.rs (NEW) — NSDraggingSource conformer (disabled)
alacritty/src/display/window.rs   — AppKit FFI (tab title, NSAlert, set_outer_position)
alacritty/src/display/mod.rs      — TabActivity enum, apply_tab_title composition
alacritty/src/event.rs            — RenameTabState, DragCandidate, restore on launch
alacritty/src/input/mod.rs        — tab_swipe_step, drag-out plumbing, ClearInputLine
alacritty/src/input/keyboard.rs   — double-Esc detection, rename input routing
alacritty/src/config/bindings.rs  — RENAME_TAB BindingMode, new actions
alacritty/src/macos/proc.rs       — has_children() via proc_listpids
alacritty/src/window_context.rs   — session_snapshot, restored_size/position application
alacritty/src/scheduler.rs        — Topic::TabActivity, Topic::SessionSave
alacritty/src/cli.rs              — restored_* fields on WindowOptions
alacritty/Cargo.toml              — objc2-app-kit features (NSAlert, NSDragging, etc.)
```

## Maintainership

This fork is unrelated to upstream maintainership. Issues with these
additions: file under the user's own fork; do **not** open them against
[alacritty/alacritty](https://github.com/alacritty/alacritty). For
upstream-only behavior, restore `/Applications/Alacritty.app/Contents/MacOS/alacritty.orig`
in place of the patched binary.
