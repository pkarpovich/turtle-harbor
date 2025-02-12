# Turtle Harbor

Turtle Harbor is a macOS daemon for managing scripts with automatic restart capabilities and cron scheduling. It
provides a Docker-like experience with familiar CLI commands (`th up`, `th down`, `th ps`) and YAML configuration,
making it easy to manage local scripts and processes with the same patterns you use for containers.

## Features

- Automatic restart of failed processes
- Cron-based scheduling
- Process status monitoring
- Per-script logging
- Robust state management
- Simple CLI interface

## Installation

### Via Homebrew

```bash
brew install pkarpovich/apps/turtle-harbor
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
# Start the daemon
turtled

# Or using brew services
brew services start turtle-harbor
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

## Logging

Logs are stored in:

- Development: `./logs/{script_name}.log`
- Production: `/opt/homebrew/var/log/turtle-harbor/{script_name}.log`

## State Management

State is persisted in:

- Development: `/tmp/turtle-harbor-state.json`
- Production: `/opt/homebrew/var/lib/turtle-harbor/state.json`

## Contributing

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## Development

### Prerequisites

- Rust (latest stable)
- macOS
- Cargo

### Building

```bash
# Install dependencies
cargo fetch

# Build in debug mode
cargo build

# Build in release mode
cargo build --release
```

### Testing

```bash
cargo test
```

## License

[MIT License](LICENSE)