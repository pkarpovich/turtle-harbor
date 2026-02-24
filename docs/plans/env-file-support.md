# Add env_file support to script config

## Overview

Scripts managed by turtle-harbor need secrets (API keys, tokens) that shouldn't be hardcoded in `scripts.yml`. Adding an `env_file` field lets scripts load environment variables from a `.env` file, keeping secrets out of version control.

Currently env vars can only be set inline in `scripts.yml` via the `env` field. This adds a complementary `env_file` field that loads KEY=VALUE pairs from a file on disk.

## Context

- `src/common/config.rs` — `Script` struct with `env` field and `resolved_env()` method
- `src/common/error.rs` — error enum with `thiserror`
- `src/daemon/process_supervisor.rs:80` — calls `resolved_env()` and passes to `cmd.envs()`
- No external .env crate in Cargo.toml — parse ourselves (simple KEY=VALUE format)
- No existing tests in the project

## Development Approach

- **Testing approach**: Regular (code first, then tests)
- Minimal change — one new field, one new parser function, update one existing method
- Self-contained .env parser using stdlib only (no new crate dependency)
- Backward compatible — `env_file` is optional, existing configs work unchanged

## Testing Strategy

- **Unit tests**: inline `#[cfg(test)]` module in `config.rs` for `parse_env_file`
- Test parsing: comments, empty lines, quoted values, KEY=VALUE, edge cases
- Test `resolved_env()` priority: env_file loaded first, inline env overrides

## Implementation Steps

### Task 1: Add env_file .env parser to config module

- [x] add `parse_env_file(path: &Path) -> HashMap<String, String>` to `src/common/config.rs`
  - reads file line by line
  - skips empty lines and `#` comments
  - parses `KEY=VALUE`, strips surrounding quotes (`"` and `'`) from values
  - handles `KEY=` (empty value) and `KEY` without `=` (skip)
  - logs warning on malformed lines, does not fail
- [x] write tests for parse_env_file: basic KEY=VALUE parsing
- [x] write tests for parse_env_file: comments, empty lines, quoted values, empty values
- [x] write tests for parse_env_file: malformed lines (no panic, returns partial result)
- [x] `cargo test` — must pass before task 2

### Task 2: Add env_file field to Script and integrate into resolved_env

- [x] add `env_file: Option<PathBuf>` with `#[serde(default)]` to `Script` struct in `src/common/config.rs`
- [x] update `resolved_env()`: if `env_file` is Some, resolve path relative to context (or cwd), call `parse_env_file`, insert all entries first, then overlay inline `env` on top
- [x] add `EnvFileRead` variant to `src/common/error.rs` (for file not found — log warning but don't crash)
- [x] write tests for resolved_env with env_file: verify env_file vars loaded
- [x] write tests for resolved_env priority: inline env overrides env_file
- [x] `cargo test` — must pass before task 3

### Task 3: Update examples and verify

- [x] update `examples/scripts.yml` — add `env_file: ".env"` to radio-t-checker, remove RELAY_SECRET from inline env
- [x] create `scripts/.env.example` with placeholder keys
- [x] add `.env` to `.gitignore` if not already there
- [x] `cargo build` — must compile
- [x] `cargo test` — all tests pass

## Technical Details

**.env format supported:**
```
# comment line
KEY=value
KEY="double quoted value"
KEY='single quoted value'
KEY=           # empty value
               # blank lines skipped
```

**Priority order in resolved_env() (later overrides earlier):**
1. `env_file` variables
2. inline `env` from scripts.yml
3. `venv` PATH/VIRTUAL_ENV (already last in current code)

**Parse behavior:**
- File not found → log warning, continue with empty HashMap (non-fatal)
- Malformed line → log warning, skip line, continue parsing
- No variable expansion, no multiline values, no `export` prefix handling

## Post-Completion

**Manual verification:**
- Create `scripts/.env` with `RELAY_SECRET=test-value`
- Run `th up` with config using `env_file: ".env"`
- Verify script receives RELAY_SECRET in its environment
