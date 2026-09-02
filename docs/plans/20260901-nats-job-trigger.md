# NATS job trigger: run a script per JetStream message with ack/nak by exit code

## Overview

Today turtle-harbor knows two ways to run a script: keep a long-lived process alive (`restart_policy`) or start it on a `cron` schedule. This plan adds a third trigger, `nats:`, that turns a script into a JetStream pull-consumer job: one message arrives, one process runs with the message in its environment, and the process exit code decides whether the message is acknowledged, retried later, or terminated. Optionally the daemon publishes the job's result to another subject before acknowledging.

The motivating consumer is `podcast-transcriber` (a Python ASR pipeline in a separate repository) which currently carries its own 350-line NATS consumer, its own retry loop, its own Loki pusher and a permanently resident 3.5 GB process. With this trigger it becomes a one-shot CLI that knows nothing about NATS, and every queue concern - durable consumer, ack/nak, retry delay, heartbeat, timeout, restart on outage, logs, Loki, launchd - lives in the daemon that already does the rest of that for every other script.

The abstraction is deliberately the same as cron: a trigger is a tokio task that waits for something and sends an event into `DaemonCore`. Cron waits for a clock. This one waits for a message and additionally needs the outcome back, which the codebase already expresses as a `oneshot` reply on `DaemonEvent::ClientCommand`.

### Non-goals

- No routing inside the daemon by message content. One `nats:` block maps one subject filter to one command; anything that depends on fields inside the payload is the job's business.
- No stream creation. Both the consumed stream and the stream that captures the `publish` subject must exist; a missing consumed stream fails consumer setup and the listener retries every 30 seconds, and a publish with no capturing stream is a publish error (`Nak`).
- No changes to how long-lived or cron scripts behave. A script without `nats:` is byte-for-byte unaffected.
- No tracing inside the daemon. The `traceparent` header is forwarded verbatim into the job environment and onto the published result; the daemon creates no spans.
- No Telegram or other notifications from the daemon. Failures are visible through `th ps`, the per-script log, Loki and `/health`; the job itself may notify.
- No integration test against a live NATS server in `cargo test`. Pure parts are unit-tested; the end-to-end path is the manual acceptance run in Post-Completion.
- No replay of history when a durable is first created (`DeliverPolicy::New`), and no configuration knob for that in v1.
- No reconciliation of an existing durable's server-side configuration from YAML. Binding returns the consumer as it is on the server; the daemon reads the server's values and warns on mismatch, it never rewrites them.

### Rejected alternatives

- A separate queue-runner daemon: duplicates process supervision, logging, Loki shipping, launchd install and self-update that this daemon already has, and creates a second place scripts have to be registered.
- Parsing the job's stdout for a result: stdout is already consumed line by line by the script logger and shipped to Loki; a result file at a daemon-chosen path is unambiguous and keeps logs as logs.
- Passing the payload on stdin: `ProcessSupervisor` does not pipe stdin today and the payload is a few hundred bytes; an environment variable rides the existing `resolved_env` path with no new plumbing.
- Acknowledging a permanently failed message: JetStream has `AckKind::Term` for exactly "not processed, do not redeliver"; using it keeps the consumer's numbers honest.
- Aborting the listener task on stop like cron does: an in-flight message would stay unacknowledged until `ack_wait` expires (30 minutes in production). A `watch`-based shutdown lets the listener `Nak` the message with zero delay so it is redelivered as soon as the consumer is back.
- `fetch()` for pulling: in async-nats 0.50 `FetchBuilder` sends the pull request with `no_wait: true`, so an idle subject answers with a 404 status immediately and the loop would spin against the server. `batch()` sends `no_wait: false` and the server holds the request open for `expires`.
- Deciding retry exhaustion from the YAML `max_deliver`: the server enforces its own stored value, and binding an existing durable ignores the passed config, so the daemon reads the server's value after binding and decides from that.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - every file under `src/` must follow it: for loops over iterator chains, `let ... else`, shadowing, newtypes, enums over bools, no wildcard match, no `matches!`, explicit destructuring, no comments. The repository has zero doc comments and stays that way; do not add `///` on new public items.

## Context (from discovery)

Files involved:
- `src/common/config.rs` - `Settings { log_dir, loki }`, `Script { command, restart_policy, max_restarts, cron, context, venv, env, env_file }`, `resolved_env`, `Config::load`. Gains `Settings.nats`, `Script.nats`, and validation.
- `src/common/ipc.rs` - `ProcessStatus { Running, Stopped, Failed, Restarting }` is serialized into `state.json` and rendered by `th`. Gains `Listening`.
- `src/bin/th.rs` - `format_status` renders `ProcessStatus`. Gains the `Listening` arm.
- `src/common/error.rs` - `thiserror` enum. Gains the config-validation, duplicate-durable and job-result variants.
- `src/client/error.rs` - `handle_error` matches every `Error` variant with no `_` arm (lines 5-33). Every new variant needs an arm here or the crate does not build.
- `src/common/paths.rs` - profile-split path helpers. Gains a jobs directory for result files.
- `src/daemon/process_supervisor.rs` - `start_script(name, script_def, broadcast_tx, config_dir)` spawns `sh -c` with `setpgid(0,0)`, injects `resolved_env`, reports exit through `DaemonEvent::ProcessExited { name, instance_id, status }`. Already refuses to start a second instance (`ScriptStartResult::AlreadyRunning`). `stop_script` removes the map entry first (line 142), then signals and awaits the watcher for up to 5 + 2 seconds. Gains an options struct and extra env.
- `src/daemon/daemon_core.rs` - the actor: one event loop over `mpsc::Receiver<DaemonEvent>` (capacity 256, FIFO), all handlers on `&mut self`. `handle_cron_tick` is the template for a triggered start; `handle_process_exit` drops events whose `instance_id` no longer matches the supervisor entry (line 332), then computes exit code, updates health and persists `Stopped`/`Failed` (lines 365-375); `start_script`, `stop_script`, `restore_state` (restart loop at 812-815 keyed on `Running | Restarting`, cron re-schedule loop at 832-853 keyed on config), `reload_config`, `forget_script`, `shutdown` each touch `self.cron` and must touch the new manager the same way. `matches!` appears six times in this file; that is baseline, not something this plan touches.
- `src/daemon/cron_manager.rs` + `src/daemon/scheduler.rs` - the shape to mirror: `HashMap<String, JoinHandle<()>>`, `schedule`/`cancel`/`cancel_all`/`is_scheduled`, one spawned task per script sending `DaemonEvent::CronTick { name }`. Note `cancel_all` aborts without waiting - the NATS manager must not copy that.
- `src/daemon/config_manager.rs` - `ConfigDiff { added, removed, changed }` from `reload`, comparing `Script` values only; settings changes are invisible to it today. `has_script_globally` is the cross-config lookup the duplicate-durable rule needs.
- `src/daemon/state.rs` - `RunningState` persistence (tmp+rename); uses `ProcessStatus` as a field, never matches it.
- `src/daemon/server.rs` - already uses `tokio::sync::watch` for http shutdown; the same primitive drives listener shutdown.
- `Cargo.toml` - gains `async-nats`, `futures-util`, `humantime-serde`.

Related patterns found:
- Tests live in `#[cfg(test)] mod tests` at the bottom of each file. `daemon_core.rs` has `make_core()` building a `DaemonCore` on a `TempDir`, `write_config(&str)` and `dummy_script_state`; `config.rs` and `config_manager.rs` build configs from inline YAML strings; `state.rs` has four `load_*` round-trip tests.
- `Script` derives `PartialEq` because `ConfigManager::reload` diffs old and new definitions; every new nested config type must derive it too.
- State is persisted through `RunningState::update_script` (atomic tmp+rename) on every transition.
- The tree is not `rustfmt`-clean at HEAD (`cargo fmt --check` reports 41 diffs across 12 files; nothing in CI checks formatting). Task 1 fixes that in its own commit so every later gate is judged against a green tree.

Dependencies identified (verified 2026-09-01 against docs.rs and the `async-nats/v0.50.0` source):
- `async-nats` 0.50: `ConnectOptions::new().retry_on_initial_connect().connect(url)`; `jetstream::new(client)`; `Context::get_stream(name)`; `Stream::get_or_create_consumer(name, pull::Config)` - creates only when the consumer is absent and otherwise returns the existing consumer while ignoring the passed config beyond pull/push compatibility; `pull::Config { durable_name, name, filter_subject, ack_wait, max_deliver, deliver_policy, ack_policy, .. }`; `DeliverPolicy::New`; `Consumer::cached_info()` exposing the server-side `config`; `Consumer::batch().max_messages(1).expires(Duration).heartbeat(Duration).messages()` returning a `Stream` of messages (`no_wait: false`, the server holds the request open); `Message::ack_with(AckKind)` - fire-and-forget publish; `Message::double_ack_with(AckKind)` - awaits the server's confirmation; `AckKind::{Ack, Nak(Option<Duration>), Progress, Term}`; `Message::info()` exposing `delivered` and `stream_sequence`; `Message.headers: Option<HeaderMap>` with `HeaderMap::get`; `Context::publish_with_headers(subject, HeaderMap, payload)` returning `PublishAckFuture` which must itself be awaited to learn whether the stream stored the message.
- `futures-util` 0.3: `StreamExt::next` to read one message out of the batch stream (the crate is only transitive today).
- `humantime-serde` 1.1 for `"30m"`-style durations in YAML.

## Development Approach

- **Testing approach**: Regular - implementation first, then tests in the same task before moving on.
- Complete each task fully before moving to the next.
- Make small, focused changes; the existing behaviour of scripts without `nats:` must not change.
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task.
- **CRITICAL: all tests must pass before starting next task** - `cargo test` must be green.
- **CRITICAL: update this plan file when scope changes during implementation.**
- Run `cargo test` after each change.
- Run `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` before declaring a task done; Task 1 makes the tree fmt-clean so the check is meaningful from Task 2 on.
- `DaemonCore` stays an actor: nothing outside the event loop mutates its state; the listener task only sends events and awaits replies.

## Code-Quality Rules (verify before marking each task complete)

These are the `rust-style` conventions in force for this repository; a task is not done until the new code passes them.

**Control flow and matching:**
- `for` loops with mutable accumulators, not iterator chains (`filter`/`map`/`collect`/`find`/`sum`).
- `let ... else` for early exits; `if let` only for a short branch with no else.
- `match` covers every variant explicitly - no `_` arm, no `matches!` macro in new code. A `_` arm needs the user's explicit approval. The six pre-existing `matches!` calls in `daemon_core.rs` are baseline and are not rewritten by this plan.
- Structs and tuples are destructured explicitly; list the fields.

**Naming and types:**
- Shadow through transformations; no `raw_`/`parsed_` prefixes.
- Newtypes over bare `String`/`u32` where a value has a meaning (a subject, a durable name, a delivery count).
- Enums over `bool` parameters and fields.

**Signatures:**
- No function with four or more parameters (`&self`/`&mut self` excluded); past the budget use an options struct. `ProcessSupervisor::start_script` is already at four and gains a fifth input in this plan - it becomes a struct.
- Adjacent same-type parameters are a swap hazard; put them on a struct.

**Comments:**
- None. No `//` explanations, no section dividers, no TODOs, no `///` (the repository has none). The one existing exception is `// SAFETY:` on `unsafe` blocks, which stays.

**Per-task gate (before marking a checkbox `[x]`):**
1. `cargo fmt --check` clean, `cargo clippy --all-targets -- -D warnings` zero issues, `cargo test` green.
2. Run the four greps below over the lines this task added. Each must print nothing; any output is a failure to fix before ticking the box. They read the working-tree diff, so they work in every task regardless of which files exist yet.
   - four-plus parameters on one line: `git diff -U0 -- src/ | grep '^+' | grep -v '^+++' | grep -E 'fn [a-z_]+\(([^,)]*,){3,}'` - and for every new `fn` whose signature spans several lines, count its parameters by eye
   - wildcard arms: `git diff -U0 -- src/ | grep '^+' | grep -v '^+++' | grep -E '\b_ =>'`
   - `matches!`: `git diff -U0 -- src/ | grep '^+' | grep -v '^+++' | grep 'matches!'`
   - comments: `git diff -U0 -- src/ | grep '^+' | grep -v '^+++' | grep -E '^\+\s*//[^/!]' | grep -v SAFETY`
3. Only after 1-2 pass: mark complete.

## Testing Strategy

- **Unit tests**: required for every task, in `#[cfg(test)] mod tests` at the bottom of the file being changed, using the existing helpers (`make_core`, `write_config`, inline YAML).
- **Process tests**: `ProcessSupervisor` and `DaemonCore` tests may spawn real `sh -c` commands against a `TempDir` log directory; they must not depend on anything outside the repository.
- **Listener tests without a server**: `NatsManager::listen` may be exercised with a URL nothing answers on (a closed port on the loopback address); the task enters its 30-second retry loop, which is enough to test registration, `is_listening`, `cancel` and `cancel_all`. The message path itself is exercised by the acceptance run in Post-Completion, not by `cargo test`. Everything the loop delegates to - consumer config construction, outcome-to-verdict mapping, env building, header forwarding, result-file parsing - is a pure function with its own tests.
- **No e2e tests in this repo.**

## Progress Tracking

- Mark completed items with `[x]` immediately when done.
- Add newly discovered tasks with ➕ prefix.
- Document issues/blockers with ⚠️ prefix.
- Update plan if implementation deviates from original scope.

## Solution Overview

A script with a `nats:` block is a **job**. `th up` registers a listener for it and spawns nothing. The listener is a tokio task owned by a new `NatsManager` (sibling of `CronManager`) that connects, binds or creates the durable consumer, reads the server's consumer config, reports `ListenerReady`, and then loops: long-poll one message, build the job environment, send `DaemonEvent::JobTrigger { name, env, reply_tx }`, and wait on the reply while heartbeating `AckKind::Progress` every 30 seconds and enforcing `job_timeout`. `DaemonCore::handle_job_trigger` starts the process through the existing supervisor with the extra environment and parks `reply_tx` in `pending_jobs`; `handle_process_exit` resolves it with the exit status and persists `Listening` again. Back in the listener every outcome - exit status, timeout, spawn failure, lost reply, publish failure - goes through one delivery-aware `verdict` and becomes `Ack`, `Nak(nak_delay)` or `Term`.

Key decisions:
- **Exit code is the whole contract.** `0` acknowledges, `65` terminates (the job declares the message unprocessable), everything else negatively acknowledges with `nak_delay`. A crash is treated as transient, matching what the Python consumer does today with an exception.
- **Retry exhaustion is the server's number.** After binding, the listener reads `cached_info().config.max_deliver` and uses that, not the YAML, to decide `Nak` versus `Term`; a YAML value that differs is logged at warn once per session. Daemon-side failures that never handed the message to the job (`NotStarted`, a lost reply) are always `Nak` and never count toward exhaustion.
- **Long-poll, not busy-poll.** `batch()` with a 60-second expiry and a heartbeat; an idle subject costs one open request per minute.
- **Heartbeat instead of a huge `ack_wait`.** `Progress` every 30 seconds while the job runs, so `ack_wait` can be far below the job's duration without triggering redelivery.
- **`Listening` is a real state.** Between jobs `th ps` shows `listening`, never a misleading `exited (0)`; a listener that cannot reach its consumer shows `failed` in `/health`.
- **Stop never strands a message.** `th down`, `reload` and daemon shutdown flip a `watch` flag; the listener naks the in-flight message with zero delay through `double_ack_with` so the nak is confirmed before the task returns, and a restart resumes immediately.
- **New durables start from now.** `DeliverPolicy::New` on creation. Binding an existing durable keeps its position and its server-side configuration.

## Technical Details

### Configuration

```yaml
settings:
  log_dir: "./logs"
  nats:
    url: "nats://192.168.198.3:4222"

scripts:
  podcast-transcriber:
    command: "python cli.py --job"
    context: "./podcast-transcriber"
    venv: ".venv"
    env_file: ".env"
    restart_policy: "never"
    nats:
      stream: "recordings"
      subject: "recordings.completed"
      durable: "podcast-transcriber"
      ack_wait: "30m"
      max_deliver: 7
      nak_delay: "5m"
      job_timeout: "90m"
      publish: "recordings.transcribed"
```

Types (all `Debug, Clone, PartialEq, Serialize, Deserialize`):
- `NatsSettings { url: String }` on `Settings.nats: Option<NatsSettings>`.
- `NatsTrigger { stream: String, subject: String, durable: String, ack_wait: Duration, max_deliver: u32, nak_delay: Duration, job_timeout: Duration, publish: Option<String> }` on `Script.nats: Option<NatsTrigger>`. Durations deserialize through `humantime_serde`. Defaults when omitted: `ack_wait` 30m, `max_deliver` 5, `nak_delay` 5m, `job_timeout` 1h. `stream`, `subject`, `durable` are required. `ack_wait` and `max_deliver` are used only when the daemon creates the durable; for an existing durable the server's values apply.

Validation runs in `Config::load` after parsing and returns the first violation as an `Error`:
- `nats` together with `restart_policy: always` - a failed job must not be restarted without a message.
- `nats` together with `cron` - two triggers on one script.
- any script with `nats` while `settings.nats` is absent - no server to connect to.

One more rule needs the cross-config view and lives in `ConfigManager::load` and `ConfigManager::reload`, beside the existing `DuplicateScript` check: two scripts in any loaded configs must not name the same `(stream, durable)` pair - two fetch loops on one durable would split its deliveries between two commands.

### Job environment

Set on top of `resolved_env` for the one process run:

| variable | value |
|---|---|
| `TH_JOB_PAYLOAD` | message payload bytes, unmodified (UTF-8 expected; a non-UTF-8 payload is `Term`ed with an error log, never delivered) |
| `TH_JOB_SUBJECT` | the message's actual subject |
| `TH_JOB_DELIVERED` | `info().delivered` |
| `TH_JOB_MAX_DELIVER` | the server's `max_deliver` read after binding |
| `TRACEPARENT` | the `traceparent` header when the message carries one; absent otherwise |
| `TH_JOB_RESULT` | path to an empty file the daemon created, only when `publish` is configured |

### Outcomes and verdicts

`JobOutcome` is everything the listener can learn about one attempt: `Exited(i32)`, `Signaled`, `TimedOut`, `NotStarted`, `ReplyLost`, `PublishFailed`. `Delivery { delivered, max_deliver }` carries the attempt count and the server's limit. `verdict(outcome, delivery)` is the single decision point:

| outcome | verdict |
|---|---|
| `Exited(0)` | `Ack` |
| `Exited(65)` | `Term` |
| `Exited(other)`, `Signaled`, `TimedOut`, `PublishFailed` | `Nak(nak_delay)` when `delivered < max_deliver`, else `Term` |
| `NotStarted`, `ReplyLost` | `Nak(nak_delay)` regardless of `delivered` - the job never ran, so the attempt must not count |

How each outcome arises:
- `Exited`/`Signaled`: `handle_process_exit` replies with the status.
- `TimedOut`: `handle_job_timeout` replies itself (see DaemonCore changes) before stopping the process.
- `NotStarted`: `handle_job_trigger` replies on every path that does not spawn - `AlreadyRunning`, a spawn error, no config path, script gone from config, script `explicitly_stopped`, or the listener no longer registered.
- `ReplyLost`: the listener's `reply_rx` resolved with `RecvError` because the sender was dropped; a bug, logged at error level, but never a reason to lose a message.
- `PublishFailed`: `publish` is set, the job exited 0, and either stage of the publish failed (sending, or the `PublishAckFuture` reporting the stream did not store it).

Outside the table, two listener actions are not verdicts:
- exit 0 with `publish` set and a missing or non-JSON result file is a contract violation by the job: `Term` with an error log, because a retry would produce the same file.
- a stop request while a job is in flight: `double_ack_with(AckKind::Nak(Some(Duration::ZERO)))`, then return. The nak is confirmed by the server before the task ends, so daemon shutdown cannot lose it.

Every `Term` logs at error level with the script name, subject, stream sequence and reason. Health follows the existing `handle_process_exit` rule: exit 0 -> `Succeeded`, anything else -> `Failed`; a timeout is `Failed`.

### Consumer binding and the live durable

`get_or_create_consumer` binds an existing durable as it is on the server and discards the passed `pull::Config`. Therefore after binding the listener reads `cached_info().config`, takes `max_deliver` from it for `Delivery` and for `TH_JOB_MAX_DELIVER`, and logs a warn once when the YAML `ack_wait` or `max_deliver` differs from the server's. Changing those values on an existing durable is a manual `nats consumer rm` + recreate, out of scope here.

The production durable this plan is written for, captured on 2026-09-01 with `nats consumer info recordings podcast-transcriber --json`:

```json
{"durable_name":"podcast-transcriber","deliver_policy":"all","ack_policy":"explicit","ack_wait":1800000000000,"max_deliver":7,"filter_subject":"recordings.completed","max_ack_pending":1000}
```

with `num_pending 0`, `num_ack_pending 0`, `delivered.stream_seq 158`, `ack_floor.stream_seq 158`. The stream `recordings` has `subjects ["recordings.>"]` and `retention limits`, so `recordings.transcribed` is captured by the same stream and a publish to it is acknowledged by JetStream.

### Listener loop

Per script, one task in `NatsManager`:
1. `ConnectOptions::new().retry_on_initial_connect().connect(url)`; `jetstream::new(client)`; `get_stream(stream)`; `get_or_create_consumer(durable, pull::Config { durable_name, name, filter_subject, ack_wait, max_deliver, deliver_policy: New, ack_policy: Explicit })`; read `cached_info().config.max_deliver`; send `DaemonEvent::ListenerReady { name }`. Any error here or in step 2: send `DaemonEvent::ListenerFailed { name, error }` (once per failure), log at warn, sleep 30 seconds, restart from step 1 - unless the shutdown flag is set, in which case return.
2. `batch().max_messages(1).expires(60s).heartbeat(20s).messages()` and take one message with `StreamExt::next`; the stream ending with no message is a normal iteration.
3. Build `JobInput` (a non-UTF-8 payload is `Term`ed with an error log and the loop continues), `ensure_dir(jobs_dir())`, create the empty result file when `publish` is set, send `JobTrigger`.
4. `tokio::select!` over the reply, a 30-second `Progress` interval, `sleep(job_timeout)` and `shutdown.changed()`. On timeout: send `JobTimeout`, then keep waiting for the reply for at most 10 seconds, treating no reply as `TimedOut`. On shutdown: the stop action above, delete the result file, return.
5. Map the reply (`Ok(outcome)` or `Err(RecvError)` -> `ReplyLost`) through `verdict`; on `Ack` with `publish` set, read the result file, `publish_with_headers` with the forwarded traceparent, await the returned `PublishAckFuture`, and only then `ack_with(Ack)`; either publish stage failing becomes `PublishFailed` and goes back through `verdict`. Delete the result file. Log the verdict at info with script, subject, stream sequence, delivery count and outcome. Continue.

`NatsManager { tasks: HashMap<String, Listener>, event_tx }` where `Listener { handle: JoinHandle<()>, shutdown: watch::Sender<bool> }`, with `listen(name, trigger, url)`, `cancel(name)`, `cancel_all()`, `is_listening(name)`. `cancel` flips the flag, awaits the handle for at most 5 seconds, then aborts. `cancel_all` flips every flag first, then awaits all handles under one shared 5-second cap, then aborts the stragglers - it must not copy `CronManager::cancel_all`, which aborts immediately.

### DaemonCore changes

- `DaemonEvent::JobTrigger { name: String, env: HashMap<OsString, OsString>, reply_tx: oneshot::Sender<JobOutcome> }`, `DaemonEvent::JobTimeout { name: String }`, `DaemonEvent::ListenerReady { name: String }`, `DaemonEvent::ListenerFailed { name: String, error: String }`.
- `pending_jobs: HashMap<String, oneshot::Sender<JobOutcome>>` and `nats: NatsManager` on `DaemonCore`.
- `handle_job_trigger`: every path replies exactly once. Before the lookups `handle_cron_tick` does, it replies `NotStarted` when `self.nats.is_listening(name)` is false or the state entry is `explicitly_stopped` - this is what defeats a `JobTrigger` that was queued behind a `th down` on the same FIFO channel. The three early returns of the cron template (no config path, script absent, definition absent) reply `NotStarted` too. `Started` parks the sender, updates health and persists `Running`; `AlreadyRunning` or a spawn error reply `NotStarted`.
- `handle_process_exit`: inside the `instance_id` guard, after computing `exit_code`, `pending_jobs.remove(name)` and reply `Exited(code)` when the status carries a code, else `Signaled`; a missing sender is the non-job case. When the script's definition carries `nats`, the persisted status is `ProcessStatus::Listening` (with the exit code recorded) instead of `Stopped`/`Failed`, while health keeps the existing `Succeeded`/`Failed` rule. Runs before `should_restart`, which is always false for a job because validation forbids `always`.
- `handle_job_timeout`: `stop_script` removes the supervisor entry before the watcher's `ProcessExited` can be dispatched, so that event is dropped by the instance guard and cannot be relied on. The handler therefore does the bookkeeping itself: remove the sender from `pending_jobs` and reply `TimedOut`, set health `Failed` with `pid: None`, persist `Listening` with no exit code, then `supervisor.stop_script(name)`.
- `handle_listener_ready`: health entry `healthy: true`, state `NeverRan` if no job has run yet. `handle_listener_failed`: health `Failed`, `healthy: false`, `last_exit_code: None`; the persisted status stays `Listening` so a restart re-registers the listener.
- `start_script` (the `th up` path): when the definition has `nats`, do not call the supervisor; persist `ProcessStatus::Listening`, set health to `never_ran`, and `self.nats.listen(...)` if not already listening. When it has no `nats`, unchanged.
- `stop_script`, `forget_script`, `shutdown` (listeners cancelled before `supervisor.shutdown_all`): each mirrors the existing `self.cron.*` call with `self.nats.*`.
- `restore_state`: a stored `Listening` script is never handed to the supervisor. In the same pass that re-schedules cron (keyed on the loaded configs), re-listen every script whose definition carries `nats`, is not `explicitly_stopped`, and is not already listening - so a script whose last persisted status is `Listening` after a completed job, or `Running` after a crash mid-job, comes back either way.
- `reload_config`: the removed and changed branches cancel and re-listen like cron. Additionally `ConfigDiff` gains `nats_url_changed: bool` (old and new `settings.nats` differ), and when it is set every listener belonging to that config is cancelled and re-listened with the new URL.

### `ProcessStatus::Listening`

A new variant, persisted in `state.json`. `th ps` renders it as `listening` (dimmed). `full_status_list` reports uptime 0 and pid `-` for it. `restore_state`'s health mapping renders it as `NeverRan`. A `state.json` written by this version is not readable by an older daemon; a downgrade needs `th down` first. This is acceptable for a single-user tool and is noted in the README.

### Result file location

`paths::jobs_dir()` follows the `Profile` split of `state_file()`: `PathBuf::from("/tmp/turtle-harbor-jobs")` in Development, `data_dir().join("jobs")` in Production. `paths::result_path(name)` is `jobs_dir().join(format!("{name}.result.json"))`. The listener calls `paths::ensure_dir(&jobs_dir())` before creating the file, so a fresh install has the directory; the file is created empty before the trigger and removed after the verdict regardless of outcome.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code changes, tests, lints, docs - automatable in this repo.
- **Post-Completion** (no checkboxes): the podcast-transcriber migration in its own repository, the production cutover, the Gatus check, the release.

## Implementation Steps

### Task 1: Make the tree rustfmt-clean in its own commit

**Files:**
- Modify: every file `cargo fmt` rewrites (12 at HEAD, formatting only)

- [x] run `cargo fmt` and confirm `cargo fmt --check` is clean
- [x] confirm `git diff --stat` touches formatting only: `cargo test` still 43 green, `cargo clippy --all-targets -- -D warnings` still clean, no logic change
- [x] commit this as a standalone formatting change before any feature work, so every later task's diff shows only that task

### Task 2: Config types for `settings.nats` and `script.nats` with validation

**Files:**
- Modify: `Cargo.toml` (add `humantime-serde = "1.1"`)
- Modify: `src/common/config.rs`
- Modify: `src/common/error.rs`
- Modify: `src/client/error.rs`
- Modify: `src/daemon/config_manager.rs`

- [x] add `NatsSettings` and `Settings.nats: Option<NatsSettings>` with `#[serde(default)]`
- [x] add `NatsTrigger` with the eight fields from Technical Details, durations via `#[serde(with = "humantime_serde")]`, defaults through `#[serde(default = "...")]` functions, and `Script.nats: Option<NatsTrigger>` with `#[serde(default)]`
- [x] add `Error::JobRestartPolicy { name }`, `Error::ConflictingTriggers { name }`, `Error::NatsUrlMissing { name }`, `Error::DuplicateDurable { stream, durable, path }`, and a `handle_error` arm for each in `src/client/error.rs`
- [x] add `Config::validate(&self) -> Result<()>` called at the end of `Config::load`, checking the three per-file rules in order per script
- [x] add the cross-config `(stream, durable)` uniqueness check to `ConfigManager::load` and `ConfigManager::reload`, next to the existing `DuplicateScript` check
- [x] update the `make_script_with_env_file` test helper for the new field
- [x] write tests in `config.rs`: a full `nats` block parses with humantime durations and `publish`; omitted optional fields take the documented defaults; each of the three validation rules yields its error; a config without any `nats` still loads unchanged
- [x] write tests in `config_manager.rs`: two scripts sharing `(stream, durable)` across two configs are rejected on `load` and on `reload`; the same durable on different streams is accepted
- [x] run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` - must pass before Task 3

### Task 3: `ProcessStatus::Listening` in IPC, state and `th ps`

**Files:**
- Modify: `src/common/ipc.rs`
- Modify: `src/bin/th.rs`
- Modify: `src/daemon/daemon_core.rs` (the `full_status_list` and `restore_state` health-mapping matches)
- Modify: `src/daemon/state.rs` (tests only)

- [x] add `ProcessStatus::Listening`
- [x] add the `Listening` arm to `format_status` rendering `listening` dimmed
- [x] add the `Listening` arm wherever `ProcessStatus` is matched in `daemon_core.rs`: `full_status_list` uptime (0), `restore_state` health mapping (`NeverRan`), and the restore filter that decides what to restart (`Listening` is never restarted through the supervisor; Task 8 re-listens it)
- [x] write tests: `format_status(Listening, None)` renders `listening`; in `state.rs`, `RunningState` round-trips a `Listening` entry through `save`/`load`
- [x] run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` - must pass before Task 4

### Task 4: Pure job contract in `src/daemon/job.rs`

**Files:**
- Create: `src/daemon/job.rs`
- Modify: `src/daemon/mod.rs`

- [x] define `JobOutcome { Exited(i32), Signaled, TimedOut, NotStarted, ReplyLost, PublishFailed }`, `Verdict { Ack, Nak, Term }`, `Delivery { delivered: u32, max_deliver: u32 }`
- [x] define `pub fn verdict(outcome: JobOutcome, delivery: Delivery) -> Verdict` implementing the Outcomes and verdicts table: `Exited(0)` -> `Ack`; `Exited(65)` -> `Term`; `Exited(other)`/`Signaled`/`TimedOut`/`PublishFailed` -> `Nak` while `delivered < max_deliver`, else `Term`; `NotStarted`/`ReplyLost` -> `Nak` always
- [x] define `JobInput { payload: String, subject: String, delivery: Delivery, traceparent: Option<String>, result_path: Option<PathBuf> }` and `pub fn job_env(input: &JobInput) -> HashMap<OsString, OsString>` producing exactly the six variables from Technical Details
- [x] define `pub const EXIT_UNPROCESSABLE: i32 = 65`
- [x] write table-driven tests for `verdict` covering every `JobOutcome` at `delivered < max_deliver` and at `delivered == max_deliver`: the bounded outcomes flip to `Term` at the boundary, `Exited(0)` stays `Ack`, `Exited(65)` stays `Term`, `NotStarted` and `ReplyLost` stay `Nak`
- [x] write tests for `job_env`: all six variables present when everything is set; `TRACEPARENT` and `TH_JOB_RESULT` absent when `None`
- [x] run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` - must pass before Task 5

### Task 5: `ProcessSupervisor::start_script` takes an options struct with extra env

**Files:**
- Modify: `src/daemon/process_supervisor.rs`
- Modify: `src/daemon/daemon_core.rs` (three call sites: `handle_restart_after_backoff`, `handle_cron_tick`, `start_script`)

- [x] introduce `StartScript<'a> { name: &'a str, script: &'a Script, broadcast_tx: broadcast::Sender<String>, config_dir: &'a Path, extra_env: HashMap<OsString, OsString> }` and change `start_script` to take it
- [x] apply `extra_env` after `resolved_env` so job variables win over `env_file`/`env`
- [x] update the three existing call sites with an empty `extra_env`
- [x] write a process test: start `sh -c 'printf "%s" "$TH_PROBE" > "$OUT"'` with `extra_env` carrying `TH_PROBE` and an `OUT` path inside a `TempDir`, wait for `ProcessExited` on the event channel, assert the file content; and a second test that a `Script.env` value is overridden by `extra_env` of the same name
- [x] run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` - must pass before Task 6

### Task 6: Job and listener events in `DaemonCore`

**Files:**
- Modify: `src/daemon/daemon_core.rs`

- [x] add `DaemonEvent::JobTrigger { name, env, reply_tx }`, `JobTimeout { name }`, `ListenerReady { name }`, `ListenerFailed { name, error }` and dispatch all four in `run`
- [x] add `pending_jobs: HashMap<String, oneshot::Sender<JobOutcome>>` to `DaemonCore`; the `nats: NatsManager` field and the `is_listening` guard arrive in Task 8, this task adds every other guard and reply path
- [x] implement `handle_job_trigger` so that every path replies exactly once: reply `NotStarted` when the state entry is `explicitly_stopped`, on each of the three early returns copied from `handle_cron_tick`, on `AlreadyRunning` and on a spawn error; on `Started` park the sender, update health and persist `Running`
- [x] in `handle_process_exit`, inside the `instance_id` guard and before `should_restart`, remove the pending sender and reply `Exited(code)` or `Signaled`; when the script definition has `nats`, persist `Listening` (with the exit code) instead of `Stopped`/`Failed` while health keeps the existing rule
- [x] implement `handle_job_timeout`: remove and reply `TimedOut`, health `Failed` with `pid: None`, persist `Listening`, then `supervisor.stop_script(name)` with the error logged
- [x] implement `handle_listener_ready` and `handle_listener_failed` per DaemonCore changes
- [x] write tests with `make_core`: a `JobTrigger` for a script whose command is `sh -c 'exit 65'` resolves the reply with `Exited(65)` after the real `ProcessExited` event is fed through `handle_process_exit`, and the persisted status is `Listening` with `exit_code: Some(65)`; a `JobTrigger` for a script whose command is `sleep 5`, followed once the supervisor holds its entry by a second `JobTrigger` for the same script, replies `NotStarted` on the second while the supervisor still holds the first `instance_id`; a `JobTrigger` for an `explicitly_stopped` script replies `NotStarted` without spawning; a `JobTrigger` for a name absent from every config replies `NotStarted`; a `ProcessExited` with a stale `instance_id` leaves `pending_jobs` untouched; a `JobTimeout` for a running `sleep 30` job replies `TimedOut`, empties the supervisor, sets health `Failed` and persists `Listening`; `ListenerFailed` sets health unhealthy and `ListenerReady` clears it
- [x] run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` - must pass before Task 7
- ⚠️ the four new `DaemonEvent` variants have no producer until the listener lands, so `-D warnings` rejects them as dead code; each carries a `#[allow(dead_code)]` that **Task 8 must remove** once `NatsManager` constructs them.

### Task 7: `NatsManager` and the pure NATS helpers

**Files:**
- Modify: `Cargo.toml` (add `async-nats = "0.50"`, `futures-util = "0.3"`)
- Create: `src/daemon/nats_manager.rs`
- Modify: `src/daemon/mod.rs`
- Modify: `src/common/paths.rs` (add `jobs_dir()` and `result_path(name)`)
- Modify: `src/common/error.rs`
- Modify: `src/client/error.rs`

- [x] define `NatsManager { tasks: HashMap<String, Listener>, event_tx }` with `Listener { handle: JoinHandle<()>, shutdown: watch::Sender<bool> }`, and `new`, `listen(name, trigger, url)`, `cancel(name)`, `cancel_all()`, `is_listening(name)` with the contracts from Technical Details (`cancel_all` flips every flag, then awaits all handles under one 5-second cap, then aborts stragglers); `listen` spawns a placeholder task body in this task that only waits on the shutdown flag - Task 8 fills the loop in
- [x] define `pub fn consumer_config(trigger: &NatsTrigger) -> pull::Config` setting `durable_name` and `name` to the durable, `filter_subject`, `ack_wait`, `max_deliver` (as `i64`), `deliver_policy: New`, `ack_policy: Explicit`
- [x] define `pub fn traceparent(headers: Option<&HeaderMap>) -> Option<String>` and `pub fn publish_headers(traceparent: Option<&str>) -> HeaderMap`
- [x] define `pub fn read_result(path: &Path) -> Result<Vec<u8>>` that reads the file and requires it to parse as JSON (`serde_json::Value`), returning the raw bytes; add `Error::JobResultMissing { path }` and `Error::JobResultInvalid { path, source }` with `handle_error` arms
- [x] add `paths::jobs_dir()` and `paths::result_path(name)` per Technical Details
- [x] write tests: `consumer_config` maps every field and pins `DeliverPolicy::New`; `traceparent` returns the header when present and `None` when the map is absent or lacks it; `publish_headers` carries the value through; `read_result` accepts an object, rejects an empty file (`JobResultMissing` for absent, `JobResultInvalid` for empty) and rejects `not json`; `result_path("x")` ends in `x.result.json` under `jobs_dir()`; `NatsManager::listen` on a loopback URL nothing answers on makes `is_listening` true, `cancel` makes it false within the cap, `cancel` of an unknown name is a no-op, and `cancel_all` with two listeners flips both flags and empties the map
- [x] run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` - must pass before Task 8
- ⚠️ `NatsManager` has no caller until Task 8 wires it into `DaemonCore`, so `-D warnings` rejects the whole module (and its unread `event_tx` field) as dead code; the declaration in `src/daemon/mod.rs` carries a single `#[allow(dead_code)]` that **Task 8 must remove** once `DaemonCore` owns a `NatsManager`.
- ➕ `src/common/paths.rs` had a `#[cfg(all(test, target_os = "macos"))] mod tests`; it is now `#[cfg(test)]` with the macOS-only `daemon_bin_path` test gated individually, so `result_path` is covered on both platforms.
- ➕ the `traceparent` helper uses `?` rather than `let ... else` on both `Option`s: clippy's `question_mark` lint is deny-under-`-D warnings` and rejects the `let ... else` form there.

### Task 8: Listener loop and lifecycle wiring

**Files:**
- Modify: `src/daemon/nats_manager.rs`
- Modify: `src/daemon/daemon_core.rs`
- Modify: `src/daemon/config_manager.rs`

- [x] implement the listener task per the Listener loop section: connect with `retry_on_initial_connect`, bind the consumer, read `cached_info().config.max_deliver` (warn once when it differs from the YAML), send `ListenerReady`; long-poll with `batch().max_messages(1).expires(60s).heartbeat(20s).messages()` and `StreamExt::next`; build `JobInput`, `ensure_dir(jobs_dir())`, create the result file when `publish` is set, send `JobTrigger`; `select!` over the reply, a 30-second `Progress` interval, `sleep(job_timeout)` (send `JobTimeout`, keep waiting up to 10 seconds, no reply is `TimedOut`) and `shutdown.changed()` (`double_ack_with(Nak(Some(Duration::ZERO)))`, delete the result file, return)
- [x] map the reply through `verdict` (`Err(RecvError)` is `ReplyLost`); on `Ack` with `publish` set, `read_result`, `publish_with_headers`, await the returned `PublishAckFuture`, then `ack_with(Ack)`; either publish stage failing is `PublishFailed` and goes back through `verdict`; a `read_result` error is `Term` with an error log; delete the result file after every verdict
- [x] wrap setup and fetch in the 30-second retry loop that sends `ListenerFailed` on each failure and returns when the shutdown flag is set; every `Term` logs script, subject, stream sequence and reason at error level; every verdict logs at info with the same fields plus delivery count and outcome
- [x] remove the four `#[allow(dead_code)]` attributes on the `DaemonEvent` job/listener variants added in Task 6, now that the listener constructs them
- [x] add `nats: NatsManager` to `DaemonCore`; add the `is_listening` guard to `handle_job_trigger`; wire `start_script` (register instead of spawn for a `nats` script: status `Listening`, health `never_ran`), `stop_script`, `forget_script`, `shutdown` (listeners cancelled before `supervisor.shutdown_all`), the removed and changed branches of `reload_config`, and the `restore_state` re-listen pass keyed on config (`nats` present, not `explicitly_stopped`, not already listening) beside the cron re-schedule loop
- [x] add `nats_url_changed: bool` to `ConfigDiff` (old and new `settings.nats` differ) and, in `reload_config`, cancel and re-listen every `nats` script of that config when it is set
- [x] write tests with `make_core` (listeners use a loopback URL nothing answers on): `th up` on a `nats` script persists `Listening`, leaves the supervisor empty and reports `is_listening`; `th down` cancels the listener and persists `Stopped` with `explicitly_stopped`; a `JobTrigger` for a script that is not listening replies `NotStarted`; `reload_config` that removes the `nats` block cancels the listener; `restore_state` with a stored `Listening` entry re-listens; `restore_state` with a stored `Stopped` entry whose config carries `nats` and is not `explicitly_stopped` re-listens (the post-job case); `restore_state` with an `explicitly_stopped` entry does not; in `config_manager.rs`, `reload` with only `settings.nats.url` changed yields `nats_url_changed: true` and empty script diffs, and `reload_config` then re-listens the script
- [x] run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` - must pass before Task 9
- ➕ `handle_job_trigger`'s new `is_listening` guard means the Task 6 tests must register a listener first: `setup_job_core` now calls `register_listener` with the dead loopback URL, and `drain_process_exit` skips non-`ProcessExited` events so a `ListenerFailed` from that dead URL cannot race the assertion.
- ➕ `restore_state`'s re-listen pass also persists `ProcessStatus::Listening` (preserving the stored `exit_code`), so a job whose last stored status was `Stopped` does not report `stopped` in `th ps` while its listener is live.
- ➕ `bind` reads the server's `max_deliver` through two pure helpers, `delivered_count` and `server_max_deliver`, which map JetStream's `i64` (`-1` = unlimited) onto the `u32` that `Delivery` uses; both have their own tests.
- ➕ `await_outcome`'s `select!` is `biased` with the reply first, so a job that finished at the same instant as a stop request is settled by its exit code rather than naked and re-run.

### Task 9: Verify acceptance criteria

- [ ] verify every `JobOutcome` at both sides of the `max_deliver` boundary has a test in `job.rs`; the publish and result-file branches are covered by the Task 7 `read_result` tests and the acceptance run, the stop-nak by the acceptance run
- [ ] verify a config without `nats` produces the same `DaemonCore` behaviour as before (the 43 tests present at Task 1 are untouched and green)
- [ ] verify the four config-validation rules reject at load or reload with the documented errors
- [ ] verify shutdown order in `DaemonCore::shutdown`: `nats.cancel_all()` runs before `supervisor.shutdown_all()`
- [ ] verify `handle_job_timeout` does not depend on the watcher's `ProcessExited` (the reply and state transition happen before `stop_script`)
- [ ] run full `cargo test` - all tests green
- [ ] run `cargo clippy --all-targets -- -D warnings` - zero warnings
- [ ] run `cargo fmt --check` - clean
- [ ] run `cargo build --release` - compiles for the host
- [ ] run the Code-Quality diff greps over the full feature diff (`git diff <task-1-commit>..HEAD -U0 -- src/`) - each prints nothing

### Task 10: [Final] Update documentation and version

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `examples/scripts.yml`
- Modify: `Cargo.toml`

- [ ] add a "NATS job trigger" section to `README.md`: the config block, the environment table, the outcome table, the `Listening` status, the binding rule (server values win, YAML `ack_wait`/`max_deliver` apply only on creation), `DeliverPolicy::New`, the publish-stream prerequisite and the state-file downgrade note
- [ ] add `NatsManager`, `job.rs`, the four new `DaemonEvent` variants and the `Listening` status to the architecture section of `CLAUDE.md`
- [ ] add a commented `nats:` script to `examples/scripts.yml`
- [ ] bump `Cargo.toml` version to `0.7.0`
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only.*

**podcast-transcriber migration** (repository `tuclaw-workspace`, done by hand, no plan):
- Add `cli.py --job`: read `TH_JOB_PAYLOAD`, reuse `parse_envelope`, skip with exit 0 when `transcribe` is falsy, resolve the path through `STORAGE_MAP`, run `transcribe`, write `build_output_envelope(...)` to `TH_JOB_RESULT`, exit 0. Exit 65 for a malformed envelope, an unsupported language, an unknown storage or a missing file. Let any other exception propagate (exit 1, traceback on stderr). Read `TRACEPARENT` into the OpenTelemetry context the way headers are read today. Keep the Telegram start/success/failure messages and render the attempt as `TH_JOB_DELIVERED` of `TH_JOB_MAX_DELIVER`.
- Delete `consumer.py`, the NATS fields of `config.py`, `LokiSink` in `observability.py` (the daemon ships stdout/stderr to Loki already), `tests/test_consumer.py`, and drop `nats-py` from `pyproject.toml`; add tests for `--job` driven by environment variables and the result file.

**Production cutover** (the Mac, `scripts.yml` at the workspace root):
- The durable is bound, not recreated: `get_or_create_consumer` returns it untouched, so nothing is replayed and its server-side `ack_wait 30m` / `max_deliver 7` stay in force whatever the YAML says. Write `max_deliver: 7` and `ack_wait: "30m"` in the block anyway so the startup warn stays silent and the file documents the real values; the daemon takes `max_deliver` from the server either way.
- `th down podcast-transcriber`; edit the script to `command: "python cli.py --job"`, `restart_policy: "never"`, the `nats:` block with `publish: "recordings.transcribed"`, `NATS_URL` moved into `settings.nats.url`, `LOKI_URL` removed from `.env`; `th up podcast-transcriber`; `th ps` shows `listening` and `/health` reports the script healthy once `ListenerReady` has fired.
- Acceptance run: `uv run enqueue_transcription.py media/podcasts/<episode>.mp3` from `continuum-scripts`; expect `th ps` to show `running` with a pid, the transcript beside the source file on the NAS, the Telegram success message, `recordings.transcribed` gaining one message, `th ps` back to `listening`, and no Python process left resident. Then restart the daemon with `launchctl kickstart -k gui/$(id -u)/com.turtle-harbor.daemon` and confirm `th ps` shows `listening` again without any manual `th up` - this is the restart path the first critical finding was about.
- Rollback: `th down`, restore the previous script block, `th up`. The durable is untouched either way.

**Monitoring**:
- Add a Gatus check on the `podcast-transcriber` consumer's pending count (`nats consumer info recordings podcast-transcriber --json` -> `num_pending`), alerting when it stays above zero for longer than one job. Nothing watches consumer lag today; `ListenerFailed` in `/health` covers a dead listener but not a healthy listener facing a backlog.

**Release**:
- Tag `v0.7.0`; the release workflow builds and signs the binaries; upgrade the Mac via Homebrew and run `th install` per the existing caveat.

**Deferred**:
- mimi publishing meeting recordings to `recordings.completed` (separate repository).
- Lowering the transcriber durable's `ack_wait` now that `Progress` heartbeats exist: a manual `nats consumer rm` + recreate (safe, `DeliverPolicy::New` starts from now).
- A second consumer on `recordings.transcribed` (summary, Obsidian) - becomes one more `nats:` script.
