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

Turtle Harbor is a cross-platform daemon (macOS + Linux) for managing scripts with auto-restart, cron scheduling and NATS JetStream job triggers. It uses a client-server architecture with Unix socket IPC.

### Two Binaries

- **turtled** (`src/bin/turtled.rs`): Background daemon that manages script lifecycle
- **th** (`src/bin/th.rs`): CLI client with Docker-like commands (`up`, `down`, `ps`, `logs`, `install`, `uninstall`)

### Module Structure

- **common/**: Shared code between client and daemon
  - `config.rs`: YAML config parsing (`scripts.yml`), `.env` file parser, `resolved_env()` with priority: env_file < venv < inline env
  - `ipc.rs`: Unix socket protocol with `Command`/`Response` enums, `Profile` enum
  - `paths.rs`: Centralized cross-platform path resolution (socket, state, logs) using `dirs` crate
  - `error.rs`: Error types using `thiserror`

- **daemon/**: Server-side components
  - `server.rs`: Unix socket listener, signal handling, orchestrates other components
  - `process_manager.rs`: Core process lifecycle (start/stop/restart), state persistence
  - `process_monitor.rs`: Watches processes for auto-restart based on policy
  - `scheduler.rs`: Cron-based scheduling with global TX channel pattern
  - `state.rs`: JSON state persistence for daemon restarts
  - `log_monitor.rs`: Per-script log file management
  - `nats_manager.rs`: One tokio listener task per `nats:` script - binds the JetStream pull consumer, long-polls one message at a time, sends `JobTrigger` and turns the reply into ack/nak/term. `listen`/`cancel`/`cancel_all`/`is_listening` mirror `cron_manager.rs`, but `cancel_all` waits on the handles (under a 5s cap) instead of aborting, so an in-flight message is naked before shutdown
  - `job.rs`: Pure job contract - `JobOutcome`, `Verdict`, `Delivery`, `verdict()` (exit 0 acks, exit 65 terms, everything else naks until deliveries are exhausted), `JobInput` and `job_env()` building the `TH_JOB_*` variables

- **client/**: CLI-side components
  - `commands.rs`: Sends commands to daemon via socket
  - `error.rs`: CLI error handling
  - `service.rs`: Platform-native service install/uninstall (launchd on macOS, systemd on Linux)

### Key Patterns

- **Profile-based paths**: `cfg!(debug_assertions)` selects dev vs prod paths at compile time. Dev uses `/tmp/turtle-harbor.*`. Prod uses platform-native dirs via `dirs` crate (macOS: `~/Library/...`, Linux: `~/.local/share/...`). All path logic centralized in `common/paths.rs`
- **IPC protocol**: Length-prefixed JSON over Unix socket (4-byte LE length + JSON payload). `Command`/`Response` enums in `ipc.rs` define the full protocol
- **Global scheduler TX**: `once_cell::OnceCell` holds `mpsc::Sender` for scheduler communication. ProcessManager retrieves it via `get_scheduler_tx()` to send `ScriptUpdated`/`ScriptRemoved` messages
- **Async locks**: `tokio::sync::Mutex` for process state (`Arc<Mutex<HashMap>>`), avoids blocking the runtime
- **State restoration**: On daemon startup, previously running scripts (not `explicitly_stopped`) auto-restart and cron schedules are re-registered
- **Process output**: Separate async tasks for stdout/stderr, writing timestamped lines to per-script log files
- **Graceful shutdown**: `tokio::select!` on socket accept + SIGTERM + SIGINT, first signal triggers shutdown. `DaemonCore::shutdown` cancels listeners before stopping processes so in-flight messages are naked, not stranded
- **NATS job events**: `DaemonEvent::JobTrigger { name, env, reply_tx }`, `JobTimeout { name }`, `ListenerReady { name }` and `ListenerFailed { name, error }` carry the listener's work into the actor loop. `handle_job_trigger` parks `reply_tx` in `pending_jobs` and every non-spawning path replies `NotStarted`; `handle_process_exit` resolves it with the exit status
- **`ProcessStatus::Listening`**: A registered `nats:` script between runs. Persisted in `state.json`, rendered as `listening` by `th ps`, restored by re-registering the listener rather than spawning the process

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
    env_file: ".env"           # optional, loads KEY=VALUE pairs
    env:                       # optional, overrides env_file values
      LOG_LEVEL: "debug"
```

A `nats:` block turns a script into a job: one process run per JetStream message, ack/nak by exit code. It requires
`settings.nats.url` and excludes `cron` and `restart_policy: always`. See the README for the fields, the `TH_JOB_*`
environment and the outcome table.
