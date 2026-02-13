# Turtle Harbor

Turtle Harbor is a daemon for managing scripts with automatic restart capabilities and cron scheduling. It
provides a Docker-like experience with familiar CLI commands (`th up`, `th down`, `th ps`) and YAML configuration,
making it easy to manage local scripts and processes with the same patterns you use for containers.

Supports macOS (ARM/Intel) and Linux (ARM/x86).

## Features

- Automatic restart of failed processes
- Cron-based scheduling
- Process status monitoring
- Per-script logging
- Robust state management
- Simple CLI interface
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

| Parameter      | Description                    | Values            |
|----------------|--------------------------------|-------------------|
| command        | Command to execute             | String            |
| restart_policy | Restart policy                 | "always", "never" |
| max_restarts   | Maximum number of restarts     | Number            |
| cron           | Cron expression for scheduling | String (optional) |

### Restart Policies

- `always`: Automatically restart on failure
- `never`: No automatic restart

## File Locations

### Production

| Path | macOS | Linux |
|------|-------|-------|
| Socket | `~/Library/Application Support/turtle-harbor/daemon.sock` | `$XDG_RUNTIME_DIR/turtle-harbor.sock` |
| State | `~/Library/Application Support/turtle-harbor/state.json` | `~/.local/share/turtle-harbor/state.json` |
| Logs | `~/Library/Logs/turtle-harbor/` | `~/.local/share/turtle-harbor/logs/` |

### Development

| Path | Location |
|------|----------|
| Socket | `/tmp/turtle-harbor.sock` |
| State | `/tmp/turtle-harbor-state.json` |
| Logs | `./logs/` |

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
