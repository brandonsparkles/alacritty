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
| **Activity indicator** | A `⠿` prefix is added to the tab label whenever the shell has any direct child process (vim, codex, claude, copilot, sleep, etc.). Detection: `proc_listpids(PROC_PPID_ONLY, shell_pid)` every 500 ms — works regardless of how the subprocess manages its process group. |
| **Needs-attention indicator** | A `🔵` prefix is added when the terminal bell rings while the tab is unfocused. Cleared on focus. |
| **Close confirmation** | `Cmd+W`, `Cmd+Q`, and red-button close raise a native `NSAlert` ("Close" / "Cancel") when a foreground subprocess is running. "Close" is the default (gets the Return key); "Cancel" gets Escape. |
| **Double-Escape clears input line** | Pressing Escape twice within 400 ms emits `Ctrl-A` + `Ctrl-K` (`\x01\x0b`) to the PTY after the normal Escape, clearing the current readline / TUI prompt input. Also exposed as bindable `Action::ClearInputLine`. |
| **Session restoration** | Windows reopen on next launch with their cwd, tab group, tab title, size, and screen position preserved. Restored windows share a fresh launch-local tabbing ID so they reopen as tabs in one window without replaying stale AppKit IDs. Persisted to `~/Library/Application Support/org.alacritty/session.json`. Hold Shift at launch to opt out. Skipped when the CLI specifies `-e`, `--working-directory`, or `--title`. |
| **Per-tab AI session resume** | After session restoration, claude / copilot / codex tabs reopen at *their specific* prior conversation (not just the most-recent) and carry permissive CLI flags from `[ai_resume]` in `alacritty.toml`. Resolved via per-PID metadata: `~/.claude/sessions/<pid>.json`, `~/.copilot/logs/process-<ts>-<pid>.log`, codex argv `resume <uuid>`, and codex's `~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<uuid>.jsonl` (matched by `pbi_start_tvsec` + cwd, with per-save-tick claim set so sibling codex tabs don't collide). Saved commands are normalized on load against current TOML flags, and `codex resume --last` is not persisted or replayed for restored tabs because it collapses multiple tabs into one conversation. |
| **Cmd+A select all** | Selects the entire terminal contents (scrollback + visible area). Standard macOS shortcut, missing from upstream. |
| **Budget enforcement** | Daily focused-time cap + 02:00–08:00 Chicago sleep window. When exhausted, a fullscreen opaque NSView overlay covers the terminal, keystrokes are filtered, and the tab title shows `🔒 Xh Ym` countdown until the next 08:00 Central reset. One TOML-configured courtesy extension per day is enabled by default. See [Budget enforcement](#budget-enforcement) below. |
| **Window-title activity prefix** | The `⠿` / `🔵` / `🔒` prefixes are written to BOTH the NSWindowTab label AND the NSWindow title bar, so they're visible whether or not the user has 2+ tabs grouped (the native tab strip only renders with multi-tab groups). |

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
| `Cmd+A` | `SelectAll` (terminal contents — scrollback + visible) |
| `Cmd+Shift+Ctrl+E` | `GrantCourtesy` (spend the once-per-day courtesy budget extension) |

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

## AI Resume Flags

Resume command flags are configured in `alacritty.toml`, not hardcoded in
the restore logic:

```toml
[ai_resume.claude]
flags = ["--dangerously-skip-permissions"]

[ai_resume.codex]
flags = ["--dangerously-bypass-approvals-and-sandbox"]

[ai_resume.copilot]
flags = ["--yolo"]
```

The Rust code owns session detection and command shape (`claude --resume`,
`codex resume`, `copilot --resume=`); this section owns local permission
policy. Saved session commands are normalized through the current config
before replay, so changing TOML applies to old saved sessions on the next
launch.

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

## Budget enforcement

A commitment-device subsystem that caps how long alacritty can be used
each day. Designed for users who want to break passive-attention habits
around terminal-based AI tools.

### What it does

| Component | Trigger | Effect |
|---|---|---|
| 1-second tick | Background timer | Increments `active_seconds` while any alacritty window is focused. |
| Hide-when-inactive | After `background_grace_seconds` (default 300 s) of no focus | Calls `NSApp.hide()`. Counter stops while hidden. Counter resumes on next focus. |
| Cap exhaustion | `active_seconds >= cap_seconds` (default 10 800 = 3 h) | Block engages: input filtered, lockout overlay rendered. |
| Sleep window | Wall-clock time between `sleep_start_hour` (default 02:00) and `sleep_end_hour` (default 08:00) Chicago | Block engages regardless of remaining cap. |
| Courtesy extension | User clicks the overlay button or presses `Cmd+Shift+Ctrl+E` | One-shot per day: adds `courtesy_seconds` to the budget window, lifts the budget-exhausted block. Enabled by default; can be disabled with `allow_courtesy = false`. |
| Day boundary | Wall-clock crosses `sleep_end_hour` Chicago | `active_seconds = 0`, `courtesy_used = false`. New day. |

The overlay is a fullscreen opaque `NSView` over the GL surface — terminal
content is invisible behind it. A one-shot courtesy button appears while
the budget-exhausted block is active and the courtesy is still available;
it does not appear during the 02:00–08:00 sleep window or after it has
already been used that day. Keystrokes are filtered at the input layer
(the overlay also absorbs them via first-responder, so it's
belt-and-suspenders). Quitting + relaunching alacritty does not reset the
block — usage state persists to disk.

### Config (`[budget]` in `alacritty.toml`)

```toml
[budget]
enabled = true                  # master switch
cap_seconds = 10800             # 3 h
sleep_start_hour = 2            # 02:00 Chicago
sleep_end_hour = 8              # 08:00 Chicago; daily reset
timezone = "America/Chicago"    # any IANA name; falls back to Chicago on parse error
hide_when_inactive = true
background_grace_seconds = 300  # 5 min grace before hide
allow_courtesy = true            # one courtesy per day
courtesy_seconds = 900           # 15 min courtesy duration
```

All fields have sensible defaults; the section is optional. Set
`enabled = false` to disable the entire system (no ticking, no overlay,
no daemon).

### Persistent state — `~/Library/Application Support/org.alacritty/usage.json`

```json
{
  "date_chicago": "2026-05-22",
  "active_seconds": 4127,
  "courtesy_used": false,
  "courtesy_expires_at": null,
  "updated_at": 1779485231
}
```

Written via atomic temp-file-rename every second. Day rollover happens
automatically when `date_chicago` no longer matches "today" (per the
configured timezone + sleep_end_hour offset).

### HTTP daemon — `127.0.0.1:38121`

Spawned at startup when `[budget] enabled = true`. Single background
thread, std::net::TcpListener, hand-rolled HTTP/1.1, no external deps.

**`GET /usage`** → 200 OK, JSON:
```json
{
  "date_chicago": "2026-05-22",
  "active_seconds": 4127,
  "cap_seconds": 10800,
  "courtesy_used": false,
  "courtesy_expires_at": null,
  "updated_at": 1779485231,
  "blocked": false,
  "reason": null,                       // "sleep_window" | "budget_exhausted" | null
  "seconds_until_unlock": 0,
  "timezone": "America/Chicago"
}
```

**`POST /courtesy`** → only meaningful when `allow_courtesy = true`
- `200 OK` + updated JSON on success
- `409 Conflict` `{"error":"already_used"}` if already spent today
- `403 Forbidden` `{"error":"courtesy_disabled"}` when not explicitly enabled
- `403 Forbidden` `{"error":"sleep_window"}` during the sleep window (extension is meaningless then — block lifts at `sleep_end_hour` regardless)

**`OPTIONS *`** → 204 (CORS preflight).

CORS headers permit any localhost origin + `https://aisparkles.com` so
the pomodoro BudgetCard component can read the daemon directly from the
browser.

### Consumers

- Pomodoro BudgetCard (`brandonai` site, `resources/js/pomodoro/components/BudgetCard.tsx`) — polls `/usage` every 5 s, renders countdown + progress bar + courtesy button.
- Sparkles Pomodoro Tauri app (`~/Desktop/Projects/sparkles-pomodoro-app/`) — menu-bar tray icon polls `/usage` every 5 s, surfaces "⌛ 2h 14m" or "🔒 Xh Ym".

If alacritty isn't running, both consumers gracefully degrade to a
"Companion offline" state.

### Code locations

```
alacritty/src/budget.rs                 — runtime Budget state + day boundaries + courtesy
alacritty/src/config/budget.rs          — BudgetConfig deserialised from [budget]
alacritty/src/budget_daemon.rs          — HTTP daemon (single thread, std::net only)
alacritty/src/display/lockout_overlay.rs — NSView + NSTextField + NSButton overlay
alacritty/src/event.rs                  — BudgetTick handler, AppFocusState state machine
                                          (next_focus_state pure helper for testability)
alacritty/src/config/bindings.rs        — Action::GrantCourtesy binding
alacritty/src/input/mod.rs              — keystroke filter when block is in effect
```

Test coverage: `cargo test --release --bin alacritty` runs 125 tests
including 12 for the daemon (routing, CORS, JSON shape), 11 for the focus
state machine, and 5 for the budget model itself.

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
alacritty/src/session.rs                  (NEW)  — session persistence
alacritty/src/cli_resume.rs               (NEW)  — per-tab AI-CLI session resume resolver
alacritty/src/budget.rs                   (NEW)  — daily-usage budget state + day boundaries
alacritty/src/config/budget.rs            (NEW)  — [budget] config struct
alacritty/src/budget_daemon.rs            (NEW)  — localhost HTTP daemon (127.0.0.1:38121)
alacritty/src/display/lockout_overlay.rs  (NEW)  — fullscreen NSView lockout
alacritty/src/display/window.rs           — AppKit FFI (tab title, NSAlert, lockout install/hide)
alacritty/src/display/mod.rs              — TabActivity, apply_tab_title → both tab + window title
alacritty/src/event.rs                    — RenameTabState, BudgetTick, AppFocusState, next_focus_state
alacritty/src/input/mod.rs                — tab_swipe_step, ClearInputLine, budget keystroke filter
alacritty/src/input/keyboard.rs           — double-Esc detection, rename input routing
alacritty/src/config/bindings.rs          — RENAME_TAB BindingMode, SelectAll, GrantCourtesy
alacritty/src/macos/proc.rs               — has_children(), pid_path, start_tvsec, is_idle
alacritty/src/window_context.rs           — session_snapshot, restored_size/position application
alacritty/src/scheduler.rs                — Topic::TabActivity, SessionSave, BudgetTick, ResumeCommand
alacritty/src/cli.rs                      — restored_* fields on WindowOptions
alacritty/src/main.rs                     — spawns the budget daemon when [budget] enabled
alacritty/Cargo.toml                      — objc2-app-kit features (NSColor, NSFont, NSTextField,
                                            NSText), chrono, chrono-tz
```

## Maintainership

This fork is unrelated to upstream maintainership. Issues with these
additions: file under the user's own fork; do **not** open them against
[alacritty/alacritty](https://github.com/alacritty/alacritty). For
upstream-only behavior, restore `/Applications/Alacritty.app/Contents/MacOS/alacritty.orig`
in place of the patched binary.
