# Turtle Harbor

Turtle Harbor is a daemon for managing scripts with automatic restart capabilities and cron scheduling. It
provides a Docker-like experience with familiar CLI commands (`th up`, `th down`, `th ps`) and YAML configuration,
making it easy to manage local scripts and processes with the same patterns you use for containers.

Supports macOS (ARM/Intel) and Linux (ARM/x86).

## Features

- Automatic restart of failed processes
- Cron-based scheduling
- NATS JetStream job trigger (one message, one process run, ack/nak by exit code)
- Process status monitoring
- Per-script logging
- Robust state management
- Simple CLI interface
- Environment file support (`.env`) for secrets management
- Cross-platform service install (`th install` / `th uninstall`)

## Installation

### Via Homebrew (macOS)

```bash
brew install pkarpovich/apps/turtle-harbor
```

### From GitHub Releases

Download the latest release for your platform from [Releases](https://github.com/pkarpovich/turtle-harbor/releases), then extract and place `th` and `turtled` on your `PATH`:

```bash
tar -xzf turtle-harbor-*.tar.gz
sudo mv th turtled /usr/local/bin/
```

### From Source

```bash
cargo build --release
```

## Usage

### Configuration

Create a `scripts.yml` file in your working directory:

```yaml
settings:
  log_dir: "./logs"

scripts:
  backend:
    command: "./backend.sh"
    restart_policy: "always"
    max_restarts: 5
    cron: "0 */1 * * * * *"
    env_file: ".env"
    env:
      LOG_LEVEL: "info"
```

### Managing the Daemon

```bash
# Install as a system service (starts on boot)
th install

# With HTTP health endpoint
th install --http-port 8080

# Remove the system service
th uninstall

# Or run the daemon directly
turtled
```

### CLI Commands

```bash
# Start all scripts
th up

# Start a specific script
th up backend

# Stop a script
th down backend

# View process status
th ps

# View logs
th logs backend
```

## Configuration

### Script Parameters

| Parameter      | Description                              | Values            |
|----------------|------------------------------------------|-------------------|
| command        | Command to execute                       | String            |
| restart_policy | Restart policy                           | "always", "never" |
| max_restarts   | Maximum number of restarts               | Number (optional) |
| cron           | Cron expression for scheduling           | String (optional) |
| env_file       | Path to `.env` file for environment vars | String (optional) |
| env            | Inline environment variables             | Map (optional)    |
| context        | Working directory for the script         | String (optional) |
| venv           | Python virtualenv path                   | String (optional) |
| nats           | NATS JetStream job trigger               | Map (optional)    |

### Environment Variables

Scripts can receive environment variables from two sources:

- **`env_file`**: Path to a `.env` file (relative to `context` or working directory). Supports `KEY=VALUE` pairs, `#` comments, empty lines, and single/double quoted values.
- **`env`**: Inline key-value map in `scripts.yml`.

When both are specified, inline `env` values override `env_file` values for the same key.

### Restart Policies

- `always`: Automatically restart on failure
- `never`: No automatic restart

## NATS job trigger

A script with a `nats:` block is a **job**: the daemon binds a JetStream pull consumer and runs the command once per
message, with the message in the process environment. The exit code decides whether the message is acknowledged,
retried or terminated. Between messages the script shows as `listening` in `th ps` - it is a third trigger next to
`restart_policy` and `cron`, not a long-lived process.

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

| Field       | Description                                                     | Default    |
|-------------|-----------------------------------------------------------------|------------|
| stream      | Existing JetStream stream to consume from                       | required   |
| subject     | Filter subject for the consumer                                 | required   |
| durable     | Durable consumer name                                           | required   |
| ack_wait    | Server ack timeout, applied only when the durable is created    | `30m`      |
| max_deliver | Delivery limit, applied only when the durable is created        | `5`        |
| nak_delay   | Delay requested when a message is negatively acknowledged       | `5m`       |
| job_timeout | Wall-clock limit for one run; the process is stopped past it    | `1h`       |
| publish     | Subject the job's result is published to before acknowledging   | none       |

`settings.nats.url` is required as soon as any script has a `nats:` block. A `nats:` script may not also set
`cron:` or `restart_policy: always`, and two scripts across all loaded configs may not share the same
`(stream, durable)` pair.

### Job environment

Set on top of the script's resolved environment for the one run, overriding `env_file` and `env` on name collision:

| Variable            | Value                                                                      |
|---------------------|----------------------------------------------------------------------------|
| `TH_JOB_PAYLOAD`    | message payload, unmodified (a non-UTF-8 payload is terminated, never run) |
| `TH_JOB_SUBJECT`    | the message's actual subject                                               |
| `TH_JOB_DELIVERED`  | this message's delivery attempt count                                      |
| `TH_JOB_MAX_DELIVER`| the server's delivery limit for the consumer                               |
| `TRACEPARENT`       | forwarded verbatim when the message carries a `traceparent` header         |
| `TH_JOB_RESULT`     | path to an empty file to write the result into, only when `publish` is set |

With `publish` set, the job must write JSON to `TH_JOB_RESULT` before exiting 0; the daemon publishes those bytes
(with the traceparent forwarded) and acknowledges only after JetStream confirms the publish. A missing or non-JSON
result file terminates the message, since a retry would produce the same file. The publish subject must be captured
by an existing stream - the daemon never creates streams.

### Outcomes

| Outcome                                          | Verdict                                              |
|--------------------------------------------------|------------------------------------------------------|
| exit `0`                                         | `Ack`                                                |
| exit `65`                                        | `Term` - the job declares the message unprocessable  |
| any other exit code, signal, timeout, publish failure | `Nak` with `nak_delay`, or `Term` once deliveries are exhausted |
| the job never started, or its reply was lost     | `Nak` - the attempt does not count against the limit |

`th down`, a reload and daemon shutdown nak the in-flight message with zero delay and wait for the server to confirm,
so a stop never strands a message. While a job runs the daemon sends a progress heartbeat every 30 seconds, so
`ack_wait` can be far shorter than the job.

### Consumer binding

The consumer is created only when it is absent; an existing durable is bound as it is on the server and keeps its
position and configuration. The YAML `ack_wait` and `max_deliver` therefore apply only at creation - afterwards the
daemon reads the server's values, uses them for retry decisions and for `TH_JOB_MAX_DELIVER`, and logs a warning once
per session when the YAML disagrees. Changing them on a live durable is a manual `nats consumer rm` plus recreate.
A newly created durable starts from `DeliverPolicy::New`, so no history is replayed.

### Status and state file

`th ps` shows `listening` for a registered job between runs and `running` while the process is up. `Listening` is a
new value in `state.json`: a state file written by this version cannot be read by a daemon older than 0.7.0, so a
downgrade needs `th down` first.

## File Locations

### Production

| Path | macOS | Linux |
|------|-------|-------|
| Socket | `~/Library/Application Support/turtle-harbor/daemon.sock` | `$XDG_RUNTIME_DIR/turtle-harbor.sock` |
| State | `~/Library/Application Support/turtle-harbor/state.json` | `~/.local/share/turtle-harbor/state.json` |
| Logs | `~/Library/Logs/turtle-harbor/` | `~/.local/share/turtle-harbor/logs/` |
| Job results | `~/Library/Application Support/turtle-harbor/jobs/` | `~/.local/share/turtle-harbor/jobs/` |

### Development

| Path | Location |
|------|----------|
| Socket | `/tmp/turtle-harbor.sock` |
| State | `/tmp/turtle-harbor-state.json` |
| Logs | `./logs/` |
| Job results | `/tmp/turtle-harbor-jobs/` |

## State management

The daemon persists script state to `state.json` so it can resume scripts across restarts. State is reconciled against loaded configs:

- On daemon startup, any script in `state.json` that is not present in any loaded `scripts.yml` is pruned from both state and the in-memory health snapshot.
- On `Command::Reload`, scripts removed from config are pruned from state and health, unless they still exist in another loaded config (in which case they are only stopped).

This keeps `/health` a projection of currently-loaded configs - scripts deleted from config no longer linger as `failed`.

## Contributing

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## Development

### Prerequisites

- Rust (latest stable)
- macOS or Linux
- Cargo

### Building

```bash
cargo fetch
cargo build
cargo build --release
```

### Testing

```bash
cargo test
```

## License

[MIT License](LICENSE)
