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

Gremlin has 18 built-in tools across 7 categories:

| Category | Tools |
|---|---|
| Filesystem | `read_file`, `write_file`, `pwd`, `list_dir` |
| Git | `git_status`, `git_diff`, `git_branch` |
| Desktop (Hyprland) | `active_window`, `active_workspace`, `list_workspaces`, `list_monitors` |
| Clipboard | `clipboard` |
| Cargo | `cargo_build`, `cargo_check`, `cargo_test` |
| Shell | `recent_commands`, `kitty_cwd` |
| Hermes | `launch_hermes` (code_review, bug_fix, architecture, refactor, explain, documentation) |

## Requirements

- **Linux** with systemd (daemon mode)
- **Ollama** running locally with at least one model
- **Fish** shell (for history/context tools)
- **Kitty** terminal (for cwd detection)
- **Hyprland** (for desktop awareness tools — optional, tools fail gracefully)
- **Hermes** CLI (for coding delegation — optional)

## License

MIT