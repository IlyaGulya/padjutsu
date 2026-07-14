<p align="center">
  <img src="./docs/logo.svg" width="120" alt="padjutsu logo" />
</p>

## Padjutsu

macOS daemon that converts gamepad input into keyboard shortcuts, mouse control, and system commands — with automatic per-application profile switching. An accessibility tool for controlling your entire Mac with a gamepad.

### Highlights

- **Per‑app mappings**: Switches rules automatically when the frontmost app changes.
- **Mouse & scroll control**: Analog sticks for cursor movement and scrolling with configurable speed, gamma, smoothing, and precision mode.
- **Horizontal scroll with axis lock**: Scroll in both directions; axis lock prevents accidental drift.
- **Button chords & layers**: Bind single buttons, multi-button chords, or use triggers as layer modifiers (e.g., `lt+a`, `rt+dpad_left`).
- **Multiple action types**: `keystroke`, `tap`, `hold`, `macros`, `shell`, `click`, `hold_click`, `rawkey`.
- **Device remapping**: Per‑VID/PID logical remaps (e.g., Nintendo A/B, X/Y swap).
- **YAML profiles**: Human‑readable with versioned schema.
- **Haptics**: Optional short rumble on action.

## How it works

1. `padjutsud` (daemon) starts an SDL2 runtime (1ms polling) to enumerate controllers and emit input events.
2. A Cocoa listener tracks the frontmost app's bundle identifier.
3. On input, the active app's rules are evaluated. Matching rules generate effects.
4. Effects send key events, move the mouse, scroll, execute shell commands, or trigger rumble.

## Installation

```bash
cargo install --git https://github.com/IlyaGulya/padjutsu.git padjutsud
```

Or build from source:

```bash
just install    # cargo install --release + codesign
```

## Usage

- Place a `gc_profile.yaml` in `~/Library/Application Support/padjutsu/`.
- Run the daemon: `padjutsud run` (or configure via launchd for auto-start).
- Grant accessibility permission when prompted (System Settings → Privacy & Security → Accessibility).
- Switch applications; rules for the frontmost app apply automatically.

## Profile

The daemon searches for configuration in this order:

1. `~/Library/Application Support/padjutsu/gc_profile.yaml`
2. `~/.gc_profile.yaml`

Custom path: `padjutsud run --workspace /path/to/dir`

### Actions

| Action       | YAML key     | Behavior                                      |
|--------------|--------------|-----------------------------------------------|
| `keystroke`  | `keystroke`  | Press+release, repeats while held              |
| `tap`        | `tap`        | Single press+release, no repeat                |
| `hold`       | `hold`       | Press on down, release on up                   |
| `macros`     | `macros`     | Sequence of combos: `[cmd+a, backspace]`       |
| `shell`      | `shell`      | Execute shell command (async, non-blocking)    |
| `click`      | `click`      | Mouse click: `left`, `right`, `middle`, `double` |
| `hold_click` | `hold_click` | Mouse button hold for dragging                 |
| `rawkey`     | `rawkey`     | Raw macOS FlagsChanged event                   |

Extra properties: `vibrate`, `repeat_delay_ms`, `repeat_interval_ms`.

### Stick modes

| Mode          | Use case        | Key parameters                                |
|---------------|-----------------|-----------------------------------------------|
| `mouse_move`  | Cursor control  | `max_speed_px_s`, `gamma`, `precision_button`, `precision_multiplier`, `deadzone`, `tick_ms`, `smoothing_window_ms` |
| `scroll`      | Scrolling       | `speed_lines_s`, `gamma`, `horizontal`, `axis_lock`, `trigger_boost_max`, `trigger_boost_gamma` |
| `arrows`      | D-pad emulation | `repeat_delay_ms`, `repeat_interval_ms`       |
| `volume`      | Volume control  | `axis`, `min_interval_ms`, `max_interval_ms`  |
| `brightness`  | Brightness      | `axis`, `min_interval_ms`, `max_interval_ms`  |

### Example profile

```yaml
version: 1
shell: /bin/zsh

groups:
  browser:
    - company.thebrowser.Browser
  messaging:
    - ru.keepcoder.Telegram

rules:
  common:
    buttons:
      a:
        hold_click: left
      b:
        keystroke: escape
      start:
        tap: enter
      lb:
        rawkey: rcmd
      lt+dpad_left:
        keystroke: cmd+shift+[
        repeat_delay_ms: 300
        repeat_interval_ms: 120
      rt+dpad_left:
        shell: aerospace focus left

    sticks:
      left:
        mode: mouse_move
        deadzone: 0.15
        max_speed_px_s: 2800
        gamma: 2.0
        tick_ms: 2
        smoothing_window_ms: 12
        precision_button: lt
        precision_multiplier: 0.25
      right:
        mode: scroll
        deadzone: 0.15
        speed_lines_s: 120
        gamma: 1.5
        horizontal: true
        axis_lock: true
        trigger_boost_max: 5.0
        trigger_boost_gamma: 1.5

  $messaging:
    buttons:
      dpad_up:
        tap: cmd+shift+tab
      dpad_down:
        tap: cmd+tab
```

### Buttons and chords

Button names: `a`, `b`, `x`, `y`, `lb`/`rb`, `lt`/`rt`, `dpad_up/down/left/right`, `start`, `back`, `left_stick`/`right_stick`, `guide`.

Join with `+` for chords: `lt+a`, `rt+dpad_left`, `l2+r2+l1`.

### Key combos

Examples: `cmd+shift+l`, `option+space`, `enter`, `backspace`, `arrow_up`, `ctrl+cmd+[`.

## Permissions

The process must be allowed under System Settings → Privacy & Security → Accessibility. The first run will prompt for permission.

## Production latency metrics

Release builds keep low-overhead latency metrics enabled. Aggregated reports are
written to stderr every 60 seconds and cover SDL scheduling, dropped controller
events, timer deadline lateness, stick tick gaps, performer queueing, mouse post
cadence/deltas, and observed cursor tracking error.

```bash
just measure-latency 15              # temporary 5s reports + macOS sample
PADJUTSU_METRICS=0 padjutsud run     # disable metrics
PADJUTSU_METRICS_INTERVAL_S=300 ...  # report every five minutes
```

## License

MIT License. See `LICENSE`.
