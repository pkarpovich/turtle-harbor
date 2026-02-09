# Turtle Harbor Refactoring Plan

## Current Architecture Summary

Turtle Harbor is a macOS daemon for managing scripts with auto-restart and cron scheduling. It uses a client-server architecture with Unix socket IPC.

**Binaries:**
- `turtled` - background daemon managing script lifecycle
- `th` - CLI client with Docker-like commands (`up`, `down`, `ps`, `logs`)

**Module layout:**
- `common/` - shared IPC protocol, config parsing, error types
- `daemon/` - server, process manager, monitor, scheduler, state persistence
- `client/` - command sender, error display

---

## Phase 1: Crash Safety & Scheduler Resilience

**Effort:** Small | **Risk:** Low | **Files:** `state.rs`, `scheduler.rs`

### 1A. Atomic State Writes

**Problem:** `RunningState::save()` at `state.rs:52-58` uses `std::fs::write` directly to the target file. If the daemon crashes mid-write (or disk fills up), the state file is corrupted and all script state is lost on next restart.

**Additionally:** `std::fs::write` is synchronous and blocks the Tokio executor thread.

**Solution:** Write-then-rename pattern with async I/O.

```rust
// state.rs
async fn save(&self) -> Result<()> {
    if let Some(parent) = self.state_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = self.state_file.with_extension("tmp");
    let content = serde_json::to_string_pretty(self)?;
    tokio::fs::write(&tmp, &content).await?;
    tokio::fs::rename(&tmp, &self.state_file).await?;
    Ok(())
}
```

**Why rename is safe:** On POSIX systems, `rename()` is atomic - the destination file either has the old content or the new content, never partial. The `.tmp` file acts as a staging area.

**Ripple effects:**
- `save()` becomes `async fn` - all callers (`update_script`, `remove_script`) must also become async
- `update_script_state` in `process_manager.rs` already awaits the state lock, so the change flows naturally

### 1B. Scheduler Error Recovery

**Problem:** `scheduler.rs:127-134` - the per-script cron loop breaks on any error from `run_scheduled_script`. A single transient failure (process spawn permission error, temporary disk issue) permanently kills scheduling for that script with no recovery.

```rust
// Current - breaks loop on error, task dies forever
loop {
    if let Err(e) = run_scheduled_script(&pm, &script_state).await {
        tracing::error!(..);
        break;  // <-- script never runs again
    }
}
```

**Solution:** Continue loop on error, add backoff for repeated failures.

```rust
let mut consecutive_failures: u32 = 0;
loop {
    match run_scheduled_script(&pm, &script_state).await {
        Ok(_) => {
            consecutive_failures = 0;
        }
        Err(e) => {
            consecutive_failures += 1;
            tracing::error!(
                script = %script_state.name,
                error = ?e,
                consecutive_failures,
                "Error in scheduler task, will retry"
            );
            if consecutive_failures >= 5 {
                let backoff = Duration::from_secs(2u64.pow(consecutive_failures.min(6)));
                tokio::time::sleep(backoff).await;
            }
        }
    }
}
```

---

## Phase 2: Event-Driven Process Monitoring

**Effort:** Medium | **Risk:** Medium | **Files:** `process_monitor.rs`, `process_manager.rs`

### Problem

`process_monitor.rs:116-137` polls every process every second:

```rust
loop {
    let names = { processes_guard.keys().cloned().collect::<Vec<_>>() };
    for name in names {
        check_and_restart_if_needed(..); // locks mutex, calls try_wait()
    }
    sleep(Duration::from_secs(1)).await;
}
```

Issues:
- Wastes CPU cycles when all processes are healthy
- Up to 1s latency before detecting a crash and restarting
- Locks the shared mutex on every iteration for every process
- Scales poorly with number of managed scripts

### Solution

Spawn a dedicated watcher task per process. Each watcher `await`s the child's exit and sends an event on a channel.

```rust
// New event type
enum ProcessEvent {
    Exited {
        name: String,
        status: std::process::ExitStatus,
    },
}

// Spawned when a process starts
async fn watch_process(
    name: String,
    mut child: Child,
    event_tx: mpsc::Sender<ProcessEvent>,
) {
    match child.wait().await {
        Ok(status) => {
            let _ = event_tx.send(ProcessEvent::Exited { name, status }).await;
        }
        Err(e) => {
            tracing::error!(script = %name, error = ?e, "Error waiting for process");
        }
    }
}
```

**Key changes:**
- `ManagedProcess` no longer holds `Child` directly - the watcher task owns it
- `ManagedProcess` holds a `JoinHandle<()>` for the watcher (to abort on manual stop)
- `ProcessManager` holds an `mpsc::Sender<ProcessEvent>` to pass to watchers
- A single receiver loop replaces the polling monitor
- `process_monitor.rs` can be significantly simplified or removed entirely

**Restart flow:**
1. Process exits -> watcher sends `ProcessEvent::Exited`
2. Event handler checks restart policy + count
3. If restart needed: spawn new process + new watcher
4. If not: update state to Stopped/Failed

---

## Phase 3: Unified Event Loop (Core Refactor)

**Effort:** Large | **Risk:** High | **Files:** `server.rs`, `process_manager.rs`, `scheduler.rs`, `process_monitor.rs`

This is the keystone refactoring. It reshapes the daemon core from shared-mutex concurrency to a single-owner event loop (actor model).

### Problem

Current architecture has three independent async loops sharing state through `Arc<Mutex<_>>`:

```
accept_loop -----> Arc<Mutex<HashMap<String, ManagedProcess>>>
                        ^
monitor_loop ------/    |
                        |
scheduler_loop --------/   (via global OnceLock<Sender>)
```

Issues:
- Potential for deadlocks if lock ordering isn't maintained
- Hidden coupling through global `OnceLock<Sender<SchedulerMessage>>`
- Difficult to reason about state transitions - mutations happen from 3 places
- `get_scheduler_tx()` returns `Option` - silent failures if called before init

### Solution

Single event loop that owns all mutable state. Everything else sends events to it.

```rust
enum DaemonEvent {
    // From socket accept_loop
    ClientCommand {
        command: Command,
        reply_tx: oneshot::Sender<Response>,
    },
    // From per-process watcher tasks (Phase 2)
    ProcessExited {
        name: String,
        status: std::process::ExitStatus,
    },
    // From cron scheduler
    CronTick {
        name: String,
    },
    // From signal handler
    Shutdown,
}
```

The daemon core becomes:

```rust
struct DaemonCore {
    processes: HashMap<String, ProcessHandle>,
    state: RunningState,
    event_tx: mpsc::Sender<DaemonEvent>,  // cloned to subsystems
    event_rx: mpsc::Receiver<DaemonEvent>,
    log_dir: PathBuf,
}

impl DaemonCore {
    async fn run(&mut self) -> Result<()> {
        while let Some(event) = self.event_rx.recv().await {
            match event {
                DaemonEvent::ClientCommand { command, reply_tx } => {
                    let response = self.handle_command(command).await;
                    let _ = reply_tx.send(response);
                }
                DaemonEvent::ProcessExited { name, status } => {
                    self.handle_process_exit(&name, status).await;
                }
                DaemonEvent::CronTick { name } => {
                    self.handle_cron_tick(&name).await;
                }
                DaemonEvent::Shutdown => break,
            }
        }
        self.shutdown().await
    }
}
```

### What This Eliminates

| Removed | Replaced By |
|---------|-------------|
| `Arc<Mutex<HashMap<String, ManagedProcess>>>` | `HashMap` owned by event loop |
| `Arc<Mutex<RunningState>>` | `RunningState` owned by event loop |
| `static SCHEDULER_TX: OnceLock<Sender>` | `event_tx.clone()` passed at construction |
| `process_monitor.rs` polling loop | Per-process watcher tasks sending `ProcessExited` |
| Three separate loops with shared state | One loop, one owner |

### Connection Handler Changes

The socket accept loop becomes a thin adapter:

```rust
async fn accept_loop(listener: UnixListener, event_tx: mpsc::Sender<DaemonEvent>) {
    loop {
        let (stream, _) = listener.accept().await?;
        let tx = event_tx.clone();
        tokio::spawn(async move {
            handle_client(stream, tx).await;
        });
    }
}

async fn handle_client(mut stream: UnixStream, event_tx: mpsc::Sender<DaemonEvent>) {
    while let Ok(command) = ipc::receive_command(&mut stream).await {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = event_tx.send(DaemonEvent::ClientCommand { command, reply_tx }).await;
        if let Ok(response) = reply_rx.await {
            if ipc::send_response(&mut stream, &response).await.is_err() {
                break;
            }
        }
    }
}
```

### Scheduler Changes

The scheduler no longer needs its own message type or global sender:

```rust
struct CronScheduler {
    event_tx: mpsc::Sender<DaemonEvent>,
}

// Per-script cron task just sends CronTick events
async fn cron_loop(name: String, schedule: Schedule, tx: mpsc::Sender<DaemonEvent>) {
    loop {
        let next = schedule.upcoming(Local).next()?;
        sleep_until(next).await;
        let _ = tx.send(DaemonEvent::CronTick { name: name.clone() }).await;
    }
}
```

Managing cron tasks (spawn/abort on config change) moves into `DaemonCore` which holds `HashMap<String, JoinHandle<()>>` for active cron tasks.

### Migration Strategy

1. Create `DaemonCore` struct with the event enum
2. Move `ProcessManager` methods into `DaemonCore` one by one
3. Convert socket handler to use `oneshot` reply pattern
4. Wire up process watchers from Phase 2
5. Simplify scheduler to send `CronTick` events
6. Delete `process_monitor.rs`, simplify `scheduler.rs`
7. Delete global `OnceLock`

---

## Phase 4: Process Group Kills & Structured Errors

**Effort:** Medium | **Risk:** Low | **Files:** `process_manager.rs` (or `DaemonCore`), `error.rs`, `th.rs`

### 4A. Process Group Management

**Problem:** `Command::new("sh").arg("-c").arg(cmd)` at `process_manager.rs:159-164` creates a shell that may spawn children. `child.kill()` only kills the shell process - subprocesses become orphans.

**Solution:** Create a new process group and kill the entire group on stop.

```rust
use std::os::unix::process::CommandExt;

let mut cmd = std::process::Command::new("sh");
cmd.arg("-c").arg(command);

// Create new process group (child becomes group leader)
unsafe {
    cmd.pre_exec(|| {
        if libc::setpgid(0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    });
}

let child = cmd.spawn()?;
let pid = child.id().expect("child has pid");

// On stop - kill entire process group
unsafe { libc::killpg(pid as i32, libc::SIGTERM); }

// Wait with timeout, then force kill
tokio::time::sleep(Duration::from_secs(5)).await;
unsafe { libc::killpg(pid as i32, libc::SIGKILL); }
```

**Dependencies:** Add `libc` to `Cargo.toml`.

### 4B. Structured Error Types

**Problem:** Error variants like `Process(String)`, `Config(String)`, `Ipc(String)` lose type information. The CLI does string-based error display. `anyhow` and `thiserror` are mixed, with a fragile `downcast` in `th.rs:140`.

**Solution:** Replace string wrappers with structured variants. Drop `anyhow`.

```rust
#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Config file not found: {}", path.display())]
    ConfigNotFound { path: PathBuf },

    #[error("Config parse error: {0}")]
    ConfigParse(#[from] serde_yml::Error),

    #[error("Invalid cron expression '{}': {}", expression, source)]
    CronParse { expression: String, source: String },

    #[error("Script '{}' not found", name)]
    ScriptNotFound { name: String },

    #[error("Script '{}' is already running", name)]
    ScriptAlreadyRunning { name: String },

    #[error("Failed to spawn script '{}': {}", script, source)]
    ProcessSpawn { script: String, source: std::io::Error },

    #[error("Daemon is not running")]
    DaemonNotRunning,

    #[error("IPC message too large: {} bytes (max {})", size, max)]
    MessageTooLarge { size: u32, max: u32 },
}
```

**CLI mapping:** Each variant maps to a specific exit code and user-facing message without string parsing.

---

## Phase 5: Daemon-Owned Config & Reload

**Effort:** Medium | **Risk:** Medium | **Files:** `ipc.rs`, `server.rs`/`DaemonCore`, `th.rs`, `config.rs`

### Problem

Currently the CLI loads `scripts.yml` and sends individual `Command::Up` messages with all config fields embedded. The daemon has no concept of the config file and can't:
- Know which scripts *should* be running vs which *are* running
- Detect config changes
- Apply changes without full stop/start cycle

### Solution

The daemon loads and owns the config. The CLI sends intent, not config data.

**New IPC protocol:**

```rust
enum Command {
    Up { name: Option<String> },        // start by name (or all)
    Down { name: Option<String> },       // stop by name (or all)
    Restart { name: Option<String> },    // restart by name (or all)
    Reload,                              // re-read config, diff, apply
    Status,                              // all scripts (running + stopped)
    Logs { name: String, tail: u32 },    // last N lines
    Ping,                                // health check
}

enum Response {
    Ok,
    Error(String),
    ProcessList(Vec<ProcessInfo>),
    Logs(String),
    Pong,
}
```

**Reload logic in daemon:**

```rust
async fn handle_reload(&mut self) -> Response {
    let new_config = match Config::load(&self.config_path) {
        Ok(c) => c,
        Err(e) => return Response::Error(e.to_string()),
    };

    let running: HashSet<_> = self.processes.keys().cloned().collect();
    let configured: HashSet<_> = new_config.scripts.keys().cloned().collect();

    // Stop scripts removed from config
    for name in running.difference(&configured) {
        self.stop_process(name).await;
    }

    // Update/restart scripts whose config changed
    for name in running.intersection(&configured) {
        if self.config_changed(name, &new_config) {
            self.restart_process(name, &new_config).await;
        }
    }

    // Start newly added scripts
    for name in configured.difference(&running) {
        self.start_process(name, &new_config).await;
    }

    self.config = new_config;
    Response::Ok
}
```

**Config path:** Passed to daemon via env var `TURTLE_HARBOR_CONFIG` or CLI arg to `turtled`, defaulting to `scripts.yml`.

**CLI simplification:** `th up backend` just sends `Command::Up { name: Some("backend") }`. No more config loading in the client.

---

## Phase 6: Log Streaming with Tail/Follow

**Effort:** Medium | **Risk:** Low | **Files:** `ipc.rs`, `log_monitor.rs`, `process_manager.rs`/`DaemonCore`, `th.rs`

### Problem

`read_logs` at `process_manager.rs:360-370` reads the entire log file into a String and sends it over IPC. For long-running scripts, this can be megabytes. The only protection is the 10MB IPC size limit.

### Solution

Add `tail` semantics (last N lines) and optional `follow` mode (streaming).

**CLI:**

```rust
#[derive(Subcommand)]
enum Commands {
    Logs {
        script_name: String,
        #[arg(short = 'n', long, default_value = "100")]
        tail: u32,
        #[arg(short, long)]
        follow: bool,
    },
}
```

**Non-follow mode:** Read file from end, find last N newlines, return that slice. Efficient even for large files:

```rust
async fn read_last_n_lines(path: &Path, n: u32) -> Result<String> {
    let content = tokio::fs::read_to_string(path).await?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n as usize);
    Ok(lines[start..].join("\n"))
}
```

**Follow mode:** Keep the socket connection open. The daemon watches the log file and streams new lines as they appear:

```rust
// Response becomes a stream of chunks
enum Response {
    LogChunk(String),   // partial log data
    LogEnd,             // no more data (non-follow mode)
    // ... other variants
}
```

The client prints each `LogChunk` as it arrives and exits on `LogEnd` or Ctrl+C.

---

## Phase 7: Logging Overhaul

**Effort:** Medium | **Risk:** Low | **Files:** `log_monitor.rs`, `logging.rs`, `process_manager.rs`/`DaemonCore`, `config.rs`

### Problem

The current logging has two independent systems with several issues:

**Script output capture (`log_monitor.rs`):**
- `tokio::sync::Mutex` guarding `std::fs::File` — async mutex for sync I/O adds overhead
- Two tasks per script (stdout + stderr) contend on the same lock every line
- No buffering — one syscall per line
- No line length limit — a script outputting a 500MB line will OOM the daemon
- No rotation — log files grow unbounded
- Stdout and stderr are interleaved without stream labels

**Daemon logging (`logging.rs`):**
- Log directory hardcoded to `"logs"` — ignores profile-based paths
- `.pretty()` format on file output — multi-line format makes grep/parsing hard
- Single `EnvFilter` for both console and file layers

### Solution: Script Output Capture

Replace the dual-task-with-mutex approach with a single writer task per script, fed by an mpsc channel:

```rust
enum LogLine {
    Stdout(String),
    Stderr(String),
}

struct ScriptLogger {
    tx: mpsc::Sender<LogLine>,
    writer_handle: JoinHandle<()>,
}

impl ScriptLogger {
    fn new(log_path: PathBuf, max_line_len: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<LogLine>(1024);

        let writer_handle = tokio::spawn(async move {
            let file = OpenOptions::new()
                .create(true).append(true).open(&log_path).unwrap();
            let mut writer = BufWriter::with_capacity(8192, file);
            let mut flush_interval = tokio::time::interval(Duration::from_millis(500));

            loop {
                tokio::select! {
                    Some(line) = rx.recv() => {
                        let timestamp = Local::now()
                            .format("%Y-%m-%d %H:%M:%S%.3f");
                        let (stream, content) = match &line {
                            LogLine::Stdout(s) => ("stdout", s.as_str()),
                            LogLine::Stderr(s) => ("stderr", s.as_str()),
                        };
                        // Truncate long lines
                        let content = if content.len() > max_line_len {
                            &content[..max_line_len]
                        } else {
                            content
                        };
                        let _ = writeln!(
                            writer, "[{}] [{}] {}",
                            timestamp, stream, content
                        );
                    }
                    _ = flush_interval.tick() => {
                        let _ = writer.flush();
                    }
                }
            }
        });

        Self { tx, writer_handle }
    }
}
```

**Wiring to process stdout/stderr:**

```rust
// In process start logic
let logger = ScriptLogger::new(log_path, 8192);

if let Some(stdout) = child.stdout.take() {
    let tx = logger.tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            let _ = tx.send(LogLine::Stdout(line.trim().to_string())).await;
            line.clear();
        }
    });
}

if let Some(stderr) = child.stderr.take() {
    let tx = logger.tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            let _ = tx.send(LogLine::Stderr(line.trim().to_string())).await;
            line.clear();
        }
    });
}
```

**Benefits:**
- One writer, no mutex contention
- `BufWriter` batches disk writes — flushes every 500ms or when buffer is full
- Stdout vs stderr labeled in log output: `[2026-02-09 12:00:00.123] [stderr] error msg`
- Line length truncation prevents OOM
- Channel naturally serializes concurrent stdout/stderr

### Solution: Log Rotation

Add size-based rotation to `ScriptLogger`:

```rust
struct RotatingWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    bytes_written: u64,
    max_size: u64,       // e.g. 10MB
    max_files: u32,      // e.g. 5 (keeps script.log, script.log.1, ..., script.log.4)
}

impl RotatingWriter {
    fn maybe_rotate(&mut self) -> io::Result<()> {
        if self.bytes_written < self.max_size {
            return Ok(());
        }

        self.writer.flush()?;

        // Shift existing rotated files: .4 -> delete, .3 -> .4, .2 -> .3, .1 -> .2
        for i in (1..self.max_files).rev() {
            let from = self.path.with_extension(format!("log.{}", i));
            let to = self.path.with_extension(format!("log.{}", i + 1));
            if from.exists() {
                std::fs::rename(&from, &to)?;
            }
        }

        // Current file -> .1
        let rotated = self.path.with_extension("log.1");
        std::fs::rename(&self.path, &rotated)?;

        // Open fresh file
        let file = OpenOptions::new()
            .create(true).append(true).open(&self.path)?;
        self.writer = BufWriter::with_capacity(8192, file);
        self.bytes_written = 0;

        Ok(())
    }
}
```

### Solution: Daemon Logging Fixes

```rust
pub fn init_logging(log_dir: &Path) -> WorkerGuard {
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("turtle_harbor=info"));

    let file_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("turtle_harbor=debug"));

    let console_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .compact()                          // single-line compact format
        .with_filter(console_filter);

    // Use profile-based log_dir instead of hardcoded "logs"
    let file_appender = rolling::daily(log_dir, "turtle_harbor.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_target(true)
        .with_ansi(false)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .json()                             // structured JSON for parsing
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    guard
}
```

**Key changes:**
- `log_dir` parameter instead of hardcoded `"logs"`
- Separate filters: `info` on console, `debug` in file
- `.compact()` for console (single-line, readable)
- `.json()` for file output (structured, parseable, grep-friendly)
- Removed thread IDs, file/line from console (less noise)

### Config Extension

```yaml
settings:
  log_dir: "./logs"
  log:
    max_line_length: 8192      # truncate script output lines
    max_file_size: "10MB"      # rotate after this size
    max_files: 5               # keep last 5 rotated files
```

---

## Phase 8: Loki Integration

**Effort:** Medium | **Risk:** Low | **Files:** `config.rs`, `logging.rs`, `log_monitor.rs` (or new `loki.rs`), `Cargo.toml`

### Overview

Two distinct log streams need Loki integration:

| Stream | Source | Approach |
|--------|--------|----------|
| Daemon tracing logs | `tracing` macros in daemon code | `tracing-loki` layer |
| Script output | stdout/stderr of managed processes | Custom Loki pusher |

### Crate Selection

**For daemon tracing:** `tracing-loki-but-better` (v0.1.4)
- Drop-in tracing layer, same API as `tracing-loki` but rewritten internals
- Lower memory/CPU than original
- Uses `reqwest` 0.12, Rust 2024 edition
- Actively maintained (Dec 2025)

**For script output:** Custom minimal client using `reqwest` (already pulled in by `tracing-loki-but-better`)
- Full control over batching, labels, retry
- ~80 lines of code
- Script logs aren't tracing events, so a tracing layer doesn't apply

### Config Extension

```yaml
settings:
  log_dir: "./logs"
  loki:
    enabled: false                              # opt-in, disabled by default
    url: "http://localhost:3100"
    tenant_id: "default"                        # optional, X-Scope-OrgID header
    labels:                                     # static labels for all streams
      environment: "development"
      host: "my-mac"
    batch_size: 100                             # flush after N log lines
    flush_interval_secs: 5                      # flush every N seconds
    auth:                                       # optional authentication
      username: ""
      password: ""
```

```rust
// config.rs additions
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct LokiConfig {
    pub enabled: bool,
    pub url: String,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_flush_interval")]
    pub flush_interval_secs: u64,
    #[serde(default)]
    pub auth: Option<LokiAuth>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LokiAuth {
    pub username: String,
    pub password: String,
}

fn default_batch_size() -> usize { 100 }
fn default_flush_interval() -> u64 { 5 }
```

### 8A. Daemon Tracing -> Loki

Add `tracing-loki-but-better` as an optional tracing layer in `logging.rs`:

```rust
use tracing_loki_but_better as tracing_loki;

pub async fn init_logging(
    log_dir: &Path,
    loki_config: Option<&LokiConfig>,
) -> (WorkerGuard, Option<JoinHandle<()>>) {
    // ... existing console + file layers ...

    let loki_handle = if let Some(loki) = loki_config.filter(|l| l.enabled) {
        let mut builder = tracing_loki::builder();

        // Add static labels
        for (key, value) in &loki.labels {
            builder = builder.label(key, value)
                .expect("valid label");
        }

        builder = builder
            .extra_field("service", "turtle-harbor-daemon")
            .expect("valid field");

        if let Some(tenant) = &loki.tenant_id {
            builder = builder
                .http_header("X-Scope-OrgID", tenant)
                .expect("valid header");
        }

        let (loki_layer, loki_task) = builder
            .build_url(&loki.url)
            .expect("valid loki url");

        // Add to subscriber
        // (requires restructuring init to compose layers conditionally)

        let handle = tokio::spawn(loki_task);
        Some(handle)
    } else {
        None
    };

    (guard, loki_handle)
}
```

**Labels sent to Loki for daemon logs:**
```
{service="turtle-harbor-daemon", environment="development", host="my-mac", level="info"}
```

### 8B. Script Output -> Loki

Custom Loki pusher that integrates with the `ScriptLogger` from Phase 7:

```rust
// loki.rs - new module
use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

struct LokiEntry {
    timestamp_ns: String,
    line: String,
}

pub struct LokiPusher {
    tx: mpsc::Sender<(HashMap<String, String>, LokiEntry)>,
    task_handle: JoinHandle<()>,
}

impl LokiPusher {
    pub fn new(config: &LokiConfig) -> Self {
        let (tx, mut rx) = mpsc::channel::<(HashMap<String, String>, LokiEntry)>(4096);
        let client = Client::new();
        let url = format!("{}/loki/api/v1/push", config.url);
        let batch_size = config.batch_size;
        let flush_interval = Duration::from_secs(config.flush_interval_secs);
        let tenant_id = config.tenant_id.clone();

        let task_handle = tokio::spawn(async move {
            // Group by label set
            let mut buffer: HashMap<String, Vec<LokiEntry>> = HashMap::new();
            let mut count = 0;
            let mut flush_timer = tokio::time::interval(flush_interval);

            loop {
                let should_flush = tokio::select! {
                    Some((labels, entry)) = rx.recv() => {
                        let key = serde_json::to_string(&labels)
                            .unwrap_or_default();
                        buffer.entry(key).or_default().push(entry);
                        count += 1;
                        count >= batch_size
                    }
                    _ = flush_timer.tick() => {
                        !buffer.is_empty()
                    }
                };

                if should_flush {
                    let streams: Vec<_> = buffer.drain().map(|(labels_json, entries)| {
                        let labels: HashMap<String, String> =
                            serde_json::from_str(&labels_json)
                                .unwrap_or_default();
                        let values: Vec<Vec<String>> = entries.into_iter()
                            .map(|e| vec![e.timestamp_ns, e.line])
                            .collect();
                        serde_json::json!({
                            "stream": labels,
                            "values": values
                        })
                    }).collect();

                    let body = serde_json::json!({ "streams": streams });

                    let mut req = client.post(&url)
                        .header("Content-Type", "application/json")
                        .json(&body);

                    if let Some(tid) = &tenant_id {
                        req = req.header("X-Scope-OrgID", tid);
                    }

                    match req.send().await {
                        Ok(resp) if !resp.status().is_success() => {
                            tracing::warn!(
                                status = %resp.status(),
                                "Loki push failed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(error = ?e, "Loki push error");
                        }
                        _ => {}
                    }

                    count = 0;
                }
            }
        });

        Self { tx, task_handle }
    }

    pub async fn push(
        &self,
        labels: HashMap<String, String>,
        line: String,
    ) {
        let timestamp_ns = format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let _ = self.tx.send((labels, LokiEntry { timestamp_ns, line })).await;
    }
}
```

### Integration with ScriptLogger (Phase 7)

The `ScriptLogger` writer task gains an optional Loki sink:

```rust
// Inside ScriptLogger writer loop
Some(line) = rx.recv() => {
    let (stream, content) = match &line {
        LogLine::Stdout(s) => ("stdout", s.as_str()),
        LogLine::Stderr(s) => ("stderr", s.as_str()),
    };

    // Write to local file (always)
    let _ = writeln!(writer, "[{}] [{}] {}", timestamp, stream, content);

    // Push to Loki (if configured)
    if let Some(loki) = &loki_pusher {
        let mut labels = base_labels.clone();
        labels.insert("script".to_string(), script_name.clone());
        labels.insert("stream".to_string(), stream.to_string());
        loki.push(labels, content.to_string()).await;
    }
}
```

**Labels sent to Loki for script logs:**
```
{job="turtle-harbor", script="backend", stream="stdout", environment="development"}
```

### Graceful Degradation

Loki integration is fully optional and fire-and-forget:

- If `loki.enabled = false` (default): no `reqwest` connections, no overhead
- If Loki is unreachable: warnings logged, script output still goes to local files
- If Loki is slow: channel buffering absorbs bursts (4096 entries), drops silently if full
- Channel uses `try_send` semantics — never blocks the script logger

### Grafana Dashboard

With the labels above, useful LogQL queries:

```logql
# All logs from a specific script
{job="turtle-harbor", script="backend"}

# Only stderr across all scripts
{job="turtle-harbor", stream="stderr"}

# Daemon internal logs
{service="turtle-harbor-daemon"}

# Error-level daemon logs
{service="turtle-harbor-daemon"} | level="error"

# Script logs with restart context
{job="turtle-harbor", script="backend"} | line_format "{{.content}}"
```

---

## Dependency Changes Summary

| Phase | Add | Remove |
|-------|-----|--------|
| Phase 4A | `libc` | - |
| Phase 5 | - | `anyhow` (replaced by structured errors) |
| Phase 8 | `tracing-loki-but-better`, `reqwest` | - |
| All | - | `futures` (may no longer be needed after client connection reuse) |

Note: `reqwest` comes in as a transitive dependency of `tracing-loki-but-better`. We reuse it directly for the custom script log pusher — no additional dependency cost.

---

## File Impact Summary

| File | Phase | Change |
|------|-------|--------|
| `state.rs` | 1A | async save with atomic rename |
| `scheduler.rs` | 1B, 3 | error recovery, then simplify to CronTick events |
| `process_monitor.rs` | 2, 3 | event-driven watchers, then delete entirely |
| `process_manager.rs` | 2, 3 | split into event handlers within DaemonCore |
| `server.rs` | 3 | replace with DaemonCore event loop |
| `error.rs` | 4B | structured error variants |
| `ipc.rs` | 5, 6 | new command/response variants |
| `th.rs` | 5, 6 | simplified client, log follow mode |
| `config.rs` | 5, 7, 8 | daemon-side config loading, log settings, loki settings |
| `log_monitor.rs` | 7 | rewrite: mpsc writer, BufWriter, rotation, stream labels |
| `logging.rs` | 7, 8 | profile-aware paths, JSON file format, optional Loki layer |
| `loki.rs` (new) | 8 | custom Loki push client for script output |

---

## Testing Strategy

Each phase should include tests before moving to the next:

| Phase | Tests |
|-------|-------|
| 1 | Unit test: atomic write survives simulated crash (write, don't rename, verify old state intact) |
| 2 | Unit test: process watcher sends event on exit. Integration test: restart after crash |
| 3 | Integration test: send command via channel, verify response. Test concurrent commands |
| 4 | Test: killing process group terminates all children. Test: each error variant displays correctly |
| 5 | Integration test: config reload starts new scripts, stops removed ones, restarts changed ones |
| 6 | Test: tail returns exactly N lines. Test: follow mode streams new content |
| 7 | Test: stdout/stderr labeled correctly. Test: line truncation at limit. Test: rotation triggers at max size |
| 8 | Test: Loki push sends correct JSON format. Test: batching flushes at threshold. Test: graceful degradation when Loki is down |
