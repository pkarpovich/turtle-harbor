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
  - `config.rs`: YAML config parsing (`scripts.yml`), `.env` file parser, `resolved_env()` with priority: env_file < venv < inline env < `StartScript.extra_env` (the `TH_JOB_*` variables of a job run)
  - `ipc.rs`: Unix socket protocol with `Command`/`Response` enums, `Profile` enum
  - `paths.rs`: Centralized cross-platform path resolution (socket, state, logs) using `dirs` crate
  - `error.rs`: Error types using `thiserror`

- **daemon/**: Server-side components
  - `server.rs`: Unix socket listener, signal handling, orchestrates other components
  - `daemon_core.rs`: The actor - one `mpsc<DaemonEvent>` loop, every handler on `&mut self`, no shared process state
  - `process_supervisor.rs`: Process lifecycle (spawn/stop via `StartScript`, process-group signals)
  - `config_manager.rs`: Multi-config registry - `DuplicateScript`/`DuplicateDurable` checks, `ConfigDiff { added, removed, changed, nats_url_changed }`
  - `cron_manager.rs` / `scheduler.rs`: Cron-based scheduling, sends `DaemonEvent::CronTick`
  - `health.rs` / `http_server.rs`: `ScriptHealth` snapshot behind an `RwLock` and the `/health` endpoint
  - `state.rs`: JSON state persistence for daemon restarts
  - `log_monitor.rs`: Per-script log file management
  - `nats_manager.rs`: One tokio listener task per `nats:` script - binds the JetStream pull consumer, long-polls one message at a time, sends `JobTrigger` and turns the reply into ack/nak/term. `listen`/`cancel`/`cancel_all`/`is_listening` mirror `cron_manager.rs`, but `cancel_all` waits on the handles concurrently (under a shared 15s cap) instead of aborting, so an in-flight message is naked before shutdown. `signal_stop` raises the shutdown flag for a batch of names without waiting - every path that then cancels listeners one at a time (`stop_scripts`, the `removed`/`changed`/`nats_url_changed` loops in `reload_config`) calls it first, so the listeners settle in parallel and the actor loop blocks for one cap, not one per script The cap is sized above the worst-case settle path (a JetStream publish plus its ack, each bounded by the 5s context timeout), and every wait that can outlive it - including the 10s grace after `job_timeout` - selects on the shutdown flag, so no path is aborted mid-message. The pull batch is long-polled without an idle heartbeat: async-nats 0.50 returns `Poll::Pending` after consuming a heartbeat, losing the receiver wakeup, which would strand any message delivered later in the same batch. Each `Listener` keeps the `NatsTrigger` and URL it was spawned with, so `is_bound` can tell a live identical binding from a stale one - `DaemonCore::register_listener` skips only on an exact match and otherwise aborts the in-flight job and rebinds, which is what makes a `th up` after an edited `nats:` block or `settings.nats.url` re-consume from the subject the config now names instead of silently keeping the old one
  - `job.rs`: Pure job contract - `JobOutcome`, `Verdict`, `Delivery`, `verdict()` (exit 0 acks, exit 65 terms, everything else naks until deliveries are exhausted), `JobInput` and `job_env()` building the `TH_JOB_*` variables

- **client/**: CLI-side components
  - `commands.rs`: Sends commands to daemon via socket
  - `error.rs`: CLI error handling
  - `service.rs`: Platform-native service install/uninstall (launchd on macOS, systemd on Linux)

### Key Patterns

- **Profile-based paths**: `cfg!(debug_assertions)` selects dev vs prod paths at compile time. Dev uses `/tmp/turtle-harbor.*`. Prod uses platform-native dirs via `dirs` crate (macOS: `~/Library/...`, Linux: `~/.local/share/...`). All path logic centralized in `common/paths.rs`
- **IPC protocol**: Length-prefixed JSON over Unix socket (4-byte LE length + JSON payload). `Command`/`Response` enums in `ipc.rs` define the full protocol
- **Trigger managers**: cron schedules and NATS listeners are tokio tasks owned by `CronManager`/`NatsManager`. They never touch `DaemonCore` state - they only send `DaemonEvent`s on a cloned `mpsc::Sender` and (for jobs) await a `oneshot` reply
- **`StartScript` options**: `ProcessSupervisor::start_script` takes `StartScript { name, script, broadcast_tx, config_dir, extra_env }`. `extra_env` is merged over `resolved_env()` and wins on collision; every non-job spawn path passes `HashMap::new()`
- **State restoration**: On daemon startup, previously running scripts (not `explicitly_stopped`) auto-restart and cron schedules are re-registered
- **Process output**: Separate async tasks for stdout/stderr, writing timestamped lines to per-script log files
- **Graceful shutdown**: `tokio::select!` on socket accept + SIGTERM + SIGINT, first signal triggers shutdown. `DaemonCore::shutdown` cancels listeners before stopping processes so in-flight messages are naked, not stranded
- **NATS job events**: `DaemonEvent::JobTrigger { name, env, reply_tx }`, `JobTimeout { name }`, `ListenerReady { name }` and `ListenerFailed { name, error }` carry the listener's work into the actor loop. `handle_job_trigger` parks `reply_tx` in `pending_jobs` and every non-spawning path replies `NotStarted`; a trigger whose `reply_tx.is_closed()` is dropped outright, because its listener is already gone and the message was naked. `handle_process_exit` resolves it with the exit status, and `handle_job_timeout` is a no-op unless a pending job is still parked. `handle_listener_ready`/`handle_listener_failed` drop events for a name `NatsManager` no longer holds, so one queued behind a `th down` or a config removal cannot leave `/health` permanently red
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
