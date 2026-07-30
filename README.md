# Gremlin

**Local-first AI orchestration daemon for Linux desktops.**

Gremlin is a persistent desktop coworker that observes system events, responds when appropriate, and delegates complex coding work to [Hermes](https://hermes-agent.nousresearch.com). It's designed for Hyprland, Fish, Kitty, and Ollama.

Not a chatbot. A daemon with personality.

## Architecture

```
User
  │
  ▼
Gremlin ─── local tools (filesystem, git, cargo, clipboard, desktop)
  │
  ├── Ollama (small model: conversation, planning, routing)
  │
  └── Hermes Coding Service
        │
        └── Large coding model (Qwen Coder 80B or your choice)
```

Gremlin always attempts to solve a problem using the smallest capable resource:
1. Answer directly from context
2. Use a local tool (grep a file, check git, run cargo)
3. Delegate to Hermes for coding work

## Quick Start

```bash
# Build
cargo build --release

# Initialize config
./target/release/gremlin init

# Check Ollama connectivity
./target/release/gremlin check

# One-shot query (no daemon needed)
gremlin ask "what branch am I on?"

# Start the daemon (listens on Unix socket)
gremlin daemon

# Install as systemd user service (auto-starts on login)
gremlin service install
```

## Configuration

`~/.config/gremlin/config.toml`:

```toml
[model]
name = "llama3.2:3b"        # Small model for conversation/routing
temperature = 0.7

[ollama]
url = "http://localhost:11434"

[hermes]
binary = "hermes"             # Path to Hermes CLI
coding_model = "qwen2.5-coder:14b"

[preferences]
shell = "fish"
terminal = "kitty"
desktop = "hyprland"
```

## Tools

Gremlin has 25 built-in tools across 8 categories:

| Category | Tools |
|---|---|
| Filesystem | `read_file`, `write_file`, `pwd`, `list_dir` |
| Git | `git_status`, `git_diff`, `git_branch` |
| Desktop (Hyprland) | `active_window`, `active_workspace`, `list_workspaces`, `list_monitors` |
| Clipboard | `clipboard`, `clipboard_write` |
| Cargo | `cargo_build`, `cargo_check`, `cargo_test` |
| Shell | `recent_commands`, `kitty_cwd` |
| Hermes | `launch_hermes` (code_review, bug_fix, architecture, refactor, explain, documentation) |
| Memory | `memory_search`, `memory_fact`, `memory_pref`, `memory_recommend`, `memory_self_modify`, `memory_stats` |

## Requirements

- **Linux** with systemd (daemon mode)
- **Ollama** running locally with at least one model
- **Fish** shell (for history/context tools)
- **Kitty** terminal (for cwd detection)
- **Hyprland** (for desktop awareness tools — optional, tools fail gracefully)
- **Hermes** CLI (for coding delegation — optional)

## Sprite Viewer (Hyprland Window Rules)

The sprite-viewer (`gremlin-sprite`) renders Gremlin as a floating desktop mascot via Wayland. On Hyprland, it uses the compositor's IPC socket to roam; it remains stationary on other Wayland compositors because Wayland does not permit clients to set their own window position. Add these rules to your `~/.config/hypr/hyprland.conf`:

```ini
# Gremlin sprite — borderless floating mascot window (Hyprland ≥ 0.53)
windowrule = match:class ^(gremlin-sprite)$, float on, pin on, noborder on, noshadow on, nofocus on, noanim on, size 192 192, move 100%-220 100%-220
```

> **Hyprland < 0.53:** use the older `windowrulev2` syntax instead:
> ```ini
> windowrulev2 = float, class:^(gremlin-sprite)$
> windowrulev2 = pin, class:^(gremlin-sprite)$
> windowrulev2 = noborder, class:^(gremlin-sprite)$
> windowrulev2 = noshadow, class:^(gremlin-sprite)$
> windowrulev2 = nofocus, class:^(gremlin-sprite)$
> windowrulev2 = noanim, class:^(gremlin-sprite)$
> windowrulev2 = size 192 192, class:^(gremlin-sprite)$
> windowrulev2 = move 100%-220 100%-220, class:^(gremlin-sprite)$
> ```

| Rule | Purpose |
|---|---|
| `float` | Don't tile the sprite — keep it floating |
| `pin` | Show on all workspaces |
| `noborder` | No window decorations |
| `noshadow` | No drop shadow |
| `nofocus` | Don't steal keyboard focus |
| `noanim` | Skip window open/close animations |
| `size 192 192` | 48px sprite × 4x scale |
| `move` | Position in bottom-right corner |

Start the sprite viewer alongside the daemon:

```bash
# Terminal 1: start the daemon
gremlin daemon

# Terminal 2: start the sprite (4x scale)
sprite-viewer 4
```

> **Note:** You need sprite assets in `assets/sprites/` (a PNG sprite sheet + frame map JSON). Without these, the daemon runs fine but sprite tools are unavailable. See `assets/sprites/` for the expected file layout.

## License

MIT
