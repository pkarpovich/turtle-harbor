# Multi-Config Support for Turtle Harbor

## Overview
Turtle Harbor currently stores a single global `config_path`, which breaks when managing scripts from multiple `scripts.yml` files across different projects. This refactor makes config_path per-script, enabling true multi-project support — like docker-compose, but each project has its own `scripts.yml`.

### Problems solved
1. **Cron restore bug**: After daemon restart, cron schedules for stopped cron-scripts are not restored
2. **`th down` kills everything**: Stops ALL scripts from ALL configs instead of only current project
3. **`th ps` shows everything**: Shows all scripts globally instead of filtering by current config
4. **Default context bug**: Process inherits daemon's cwd instead of config directory when `context` is not set

### New feature
- **`th list`** command: Shows ALL scripts from all configs with config source column

## Context
- `src/daemon/state.rs` — `RunningState` has single `config_path: Option<PathBuf>`, `ScriptState` has no config reference
- `src/daemon/config_manager.rs` — stores single `config: Option<Config>`
- `src/daemon/daemon_core.rs` — `restore_state` skips Stopped scripts, only restores from single config
- `src/common/ipc.rs` — `Down`, `Ps`, `Reload` commands carry no config_path
- `src/daemon/process_supervisor.rs:75-78` — only sets `current_dir` when `context` is `Some`
- `src/bin/th.rs` — no `List` command, `Down`/`Ps`/`Reload` don't pass config_path

## Development Approach
- **testing approach**: Regular (code first, then tests)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task**
- **CRITICAL: update this plan file when scope changes during implementation**
- maintain backward compatibility with existing state.json format

## Testing Strategy
- **unit tests**: required for every task
- test state migration (old format → new format)
- test ConfigManager multi-config operations
- test name collision detection
- test command routing with config_path filtering

## Progress Tracking
- mark completed items with `[x]` immediately when done
- add newly discovered tasks with + prefix
- document issues/blockers with warning prefix
- update plan if implementation deviates from original scope

## Implementation Steps

### Task 1: Add config_path to ScriptState and migrate RunningState

**Files:**
- Modify: `src/daemon/state.rs`

- [ ] add `config_path: Option<PathBuf>` field with `#[serde(default)]` to `ScriptState`
- [ ] add migration logic in `RunningState::load`: if `ScriptState.config_path` is `None`, populate from top-level `RunningState.config_path`
- [ ] keep `RunningState.config_path` field for backward compat during load, but stop writing it on save
- [ ] write tests for loading old format state.json (without per-script config_path)
- [ ] write tests for loading new format state.json (with per-script config_path)
- [ ] write tests for migration: old format scripts get config_path from top-level field
- [ ] run tests — must pass before task 2

### Task 2: Refactor ConfigManager to multi-config

**Files:**
- Modify: `src/daemon/config_manager.rs`

- [ ] change `config: Option<Config>` to `configs: HashMap<PathBuf, Config>`
- [ ] change `config_path: Option<PathBuf>` to derived from HashMap keys
- [ ] update `load(&mut self, path: &Path)` to insert into HashMap
- [ ] update `config()` → `config(&self, config_path: &Path) -> Result<&Config>`
- [ ] update `config_dir()` → `config_dir(&self, config_path: &Path) -> PathBuf`
- [ ] update `script()` → `script(&self, config_path: &Path, name: &str) -> Option<&Script>`
- [ ] update `has_script()` → `has_script(&self, config_path: &Path, name: &str) -> bool`
- [ ] update `script_names()` → `script_names(&self, config_path: &Path) -> Vec<String>`
- [ ] update `log_dir()` → `log_dir(&self, config_path: &Path) -> Option<&Path>`
- [ ] update `loki_config()` → `loki_config(&self, config_path: &Path) -> Option<&LokiConfig>`
- [ ] update `reload()` → `reload(&mut self, config_path: &Path) -> Result<ConfigDiff>`
- [ ] add `all_config_paths(&self) -> Vec<PathBuf>` method
- [ ] add `has_script_globally(&self, name: &str) -> Option<PathBuf>` — returns config_path if name exists in any config
- [ ] write tests for multi-config load (two different configs)
- [ ] write tests for script lookup across configs
- [ ] write tests for reload of specific config
- [ ] write tests for `has_script_globally` name collision detection
- [ ] run tests — must pass before task 3

### Task 3: Update IPC protocol — Command and Response enums

**Files:**
- Modify: `src/common/ipc.rs`

- [ ] add `config_path: PathBuf` to `Command::Down`
- [ ] change `Command::Ps` to `Ps { config_path: Option<PathBuf> }` (None = show all)
- [ ] add `config_path: PathBuf` to `Command::Reload`
- [ ] add `config_path: Option<PathBuf>` field to `ProcessInfo` (for `th list` config column)
- [ ] run tests — must pass before task 4

### Task 4: Update CLI — add List command and pass config_path

**Files:**
- Modify: `src/bin/th.rs`

- [ ] add `List` variant to `Commands` enum
- [ ] update `Down` match arm to pass `config_path` in `Command::Down`
- [ ] update `Ps` match arm to pass `Some(config_path)` in `Command::Ps`
- [ ] add `List` match arm: send `Command::Ps { config_path: None }`
- [ ] update `Reload` match arm to pass `config_path` in `Command::Reload`
- [ ] add `print_process_list_table_with_config` function for `th list` output (adds CONFIG column showing last 2 path components)
- [ ] route `List` response through the new table printer
- [ ] run tests — must pass before task 5

### Task 5: Refactor DaemonCore — config_path-aware operations

**Files:**
- Modify: `src/daemon/daemon_core.rs`

This is the largest task. All script operations become config_path-aware.

- [ ] remove `self.state.config_path` usage from `handle_command` (Up no longer sets global config_path)
- [ ] update `start_scripts` to accept `config_path: &Path`, pass it through to `start_script`
- [ ] update `start_script` to accept `config_path: &Path`, use `self.config.script(config_path, name)` and `self.config.config_dir(config_path)`
- [ ] update `start_script` to pass config_path to `update_script_state`
- [ ] update `stop_scripts` to accept `config_path: Option<&Path>`: when Some, filter by config_path from state; when None, stop all
- [ ] update `stop_script` to pass config_path to `update_script_state`
- [ ] update `handle_command` for `Down { config_path, name }`: pass config_path to `stop_scripts`
- [ ] update `handle_command` for `Ps { config_path }`: filter `full_status_list` by config_path when Some
- [ ] update `handle_command` for `Reload { config_path }`: reload specific config
- [ ] update `handle_command` for `Up`: check name collisions via `config.has_script_globally()` before starting
- [ ] update `full_status_list` to include `config_path` in `ProcessInfo` (from state)
- [ ] update `update_script_state` to accept and store `config_path` in `ScriptState`
- [ ] update `sync_settings` to handle per-config log_dir and loki (use first config's settings or merge)
- [ ] update `handle_process_exit`: retrieve config_path from `self.state.scripts` by name
- [ ] update `handle_cron_tick`: retrieve config_path from `self.state.scripts` by name
- [ ] update `handle_restart_after_backoff`: retrieve config_path from `self.state.scripts` by name
- [ ] update `should_restart`: use config_path to look up script definition
- [ ] run tests — must pass before task 6

### Task 6: Fix restore_state for cron scripts

**Files:**
- Modify: `src/daemon/daemon_core.rs`

- [ ] refactor `restore_state` to group `ScriptState` by `config_path`
- [ ] for each unique config_path, call `self.config.load(config_path)`
- [ ] restore running scripts (existing behavior, now config-aware)
- [ ] register cron schedules for ALL scripts with cron expressions (not just running ones) — this is the core cron bug fix
- [ ] skip scripts with `explicitly_stopped = true` from cron registration
- [ ] run tests — must pass before task 7

### Task 7: Fix default context (Bug 3)

**Files:**
- Modify: `src/daemon/process_supervisor.rs`

- [ ] change `if let Some(context) = script_def.resolved_context(config_dir)` to always set `current_dir`
- [ ] use `script_def.resolved_context(config_dir).unwrap_or_else(|| config_dir.to_path_buf())`
- [ ] write test verifying current_dir is set to config_dir when context is None
- [ ] run tests — must pass before task 8

### Task 8: Add ScriptNameConflict error

**Files:**
- Modify: `src/common/error.rs`

- [ ] add `ScriptNameConflict { name: String, existing_config: PathBuf }` variant to Error enum
- [ ] run tests — must pass before task 9

### Task 9: Verify acceptance criteria

- [ ] verify cron schedules restore after daemon restart for stopped cron-scripts
- [ ] verify `th down` only stops scripts from current config
- [ ] verify `th ps` only shows scripts from current config
- [ ] verify `th list` shows all scripts with config column
- [ ] verify default context falls back to config directory
- [ ] verify name collision produces clear error message
- [ ] verify old state.json format migrates correctly
- [ ] run full test suite: `cargo test`

### Task 10: [Final] Update documentation

- [ ] update CLAUDE.md Architecture section to reflect multi-config support
- [ ] update Configuration section to mention multi-config behavior
- [ ] move this plan to `docs/plans/completed/`

## Technical Details

### State format migration

Old format:
```json
{
  "version": 2,
  "config_path": "/path/to/scripts.yml",
  "scripts": [
    {"name": "foo", "status": "Running", ...}
  ]
}
```

New format:
```json
{
  "version": 3,
  "scripts": [
    {"name": "foo", "config_path": "/path/to/scripts.yml", "status": "Running", ...}
  ]
}
```

Migration: on load, if script has no `config_path`, copy from top-level `config_path`.

### Config_path resolution
- CLI canonicalizes path via `std::fs::canonicalize` (already done in `th.rs:109`)
- Same file always produces same key in ConfigManager HashMap
- config_dir = parent of config_path

### Name collision check
- On `th up`, before starting scripts, check `config.has_script_globally(name)`
- If name exists in a different config_path, return `Error::ScriptNameConflict`

### th list output format
```
NAME                 PID      STATUS           UPTIME     RESTARTS  CONFIG
----------------------------------------------------------------------------------
calendar             -        exited (0)       00:00:00   0         environment/scripts
podcast-transcriber  49582    running          56:02:49   0         tuclaw-workspace
tm-backup-checker    -        exited (0)       00:00:00   0         environment/scripts
```

CONFIG shows last 2 components of config directory path.

## Post-Completion

**Manual verification:**
- test with two separate `scripts.yml` files from different projects
- test daemon restart preserves cron schedules for both configs
- test `th down` in one project doesn't affect the other
- test `th ps` vs `th list` output differences
