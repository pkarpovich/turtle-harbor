# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo test               # Run all tests
cargo test test_name     # Run a single test by name
cargo run --bin turtled  # Run daemon directly
cargo run --bin th       # Run CLI directly
```

## Architecture Overview

Turtle Harbor is a macOS daemon for managing scripts with auto-restart and cron scheduling. It uses a client-server architecture with Unix socket IPC.

### Two Binaries

- **turtled** (`src/bin/turtled.rs`): Background daemon that manages script lifecycle
- **th** (`src/bin/th.rs`): CLI client with Docker-like commands (`up`, `down`, `ps`, `logs`)

### Module Structure

- **common/**: Shared code between client and daemon
  - `config.rs`: YAML config parsing (`scripts.yml`)
  - `ipc.rs`: Unix socket protocol with `Command`/`Response` enums, profile-based socket paths
  - `error.rs`: Error types using `thiserror`

- **daemon/**: Server-side components
  - `server.rs`: Unix socket listener, signal handling, orchestrates other components
  - `process_manager.rs`: Core process lifecycle (start/stop/restart), state persistence
  - `process_monitor.rs`: Watches processes for auto-restart based on policy
  - `scheduler.rs`: Cron-based scheduling with global TX channel pattern
  - `state.rs`: JSON state persistence for daemon restarts
  - `log_monitor.rs`: Per-script log file management

- **client/**: CLI-side components
  - `commands.rs`: Sends commands to daemon via socket
  - `error.rs`: CLI error handling

### Key Patterns

- **Profile-based paths**: `cfg!(debug_assertions)` selects paths at compile time. Debug uses `/tmp/turtle-harbor.sock` and `/tmp/turtle-harbor-state.json`. Release uses `/opt/homebrew/var/` (overridable via `HOMEBREW_VAR` env)
- **IPC protocol**: Length-prefixed JSON over Unix socket (4-byte LE length + JSON payload). `Command`/`Response` enums in `ipc.rs` define the full protocol
- **Global scheduler TX**: `once_cell::OnceCell` holds `mpsc::Sender` for scheduler communication. ProcessManager retrieves it via `get_scheduler_tx()` to send `ScriptUpdated`/`ScriptRemoved` messages
- **Async locks**: `tokio::sync::Mutex` for process state (`Arc<Mutex<HashMap>>`), avoids blocking the runtime
- **State restoration**: On daemon startup, previously running scripts (not `explicitly_stopped`) auto-restart and cron schedules are re-registered
- **Process output**: Separate async tasks for stdout/stderr, writing timestamped lines to per-script log files
- **Graceful shutdown**: `tokio::select!` on socket accept + SIGTERM + SIGINT, first signal triggers shutdown

### Configuration

Scripts are defined in `scripts.yml`:
```yaml
settings:
  log_dir: "./logs"

scripts:
  my_script:
    command: "./script.sh"
    restart_policy: "always"  # or "never"
    max_restarts: 5
    cron: "0 */1 * * * * *"   # optional, 7-field cron expression
```
