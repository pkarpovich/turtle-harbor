# State Reconcile: prune orphan scripts from state and health snapshot

## Overview

When a script is removed from `scripts.yml`, its entry persists in `state.json` and in the in-memory health snapshot forever. As a result, `GET /health` keeps returning the orphan script as `failed`, dragging the whole endpoint to HTTP 503 indefinitely.

Concrete case: on the home Pi, `radio-t-checker` and `radio-t-recorder` were removed from config on 2026-04-05 (replaced by `radio-t-monitor`). Over a month later the daemon still reports them as `failed` in `/health`, breaking the Gatus uptime check and any other monitor consuming the endpoint.

Goal: make state and health a projection of currently-loaded configs. Anything not in any loaded config must be forgotten — both on disk (`state.json`) and in memory (health snapshot). Apply at daemon startup and at config reload.

## Context (from discovery)

Files involved:
- `src/daemon/daemon_core.rs` — primary changes (`forget_script`, `is_orphan`, `restore_state`, `reload_config`).
- `src/daemon/health.rs` — type reference only, no changes expected.
- `src/daemon/config_manager.rs` — already exposes `has_script_globally(name) -> Option<PathBuf>` which is the orphan oracle. No changes.
- `src/daemon/state.rs` — `State::remove_script(name)` already exists. No changes.

Related patterns found:
- Health snapshot is `Arc<RwLock<HashMap<String, ScriptHealth>>>` populated in `daemon_core.rs:678-707` (`restore_state`) and updated piecemeal in `update_health_on_start`, `handle_process_exit`.
- `reload_config` (`daemon_core.rs:868`) handles `diff.removed` by calling `stop_script + state.remove_script` but does NOT clean the health snapshot.
- Tests in this codebase live as `#[cfg(test)] mod tests` blocks at the bottom of each source file (see `config_manager.rs:115-272`).

Dependencies identified: none new. Uses existing `ConfigManager`, `State`, health primitives.

## Development Approach

- **Testing approach**: Regular — implementation first, then tests in the same task before moving on.
- Complete each task fully before moving to the next.
- Make small, focused changes.
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task.
- **CRITICAL: all tests must pass before starting next task** — `cargo test` must be green.
- **CRITICAL: update this plan file when scope changes during implementation.**
- Run `cargo test` after each change.
- Run `cargo clippy --all-targets -- -D warnings` before declaring a task done.
- Maintain backward compatibility: orphan pruning must not touch scripts present in any loaded config (multi-config safety).

## Testing Strategy

- **Unit tests**: required for every task. Tests go into `#[cfg(test)] mod tests` at the bottom of `daemon_core.rs` (or a new `tests/` integration test if mocking the runtime is too heavy — decide in Task 2).
- **No e2e tests in this repo.** Manual smoke test on the Pi after deploy is in Post-Completion.

## Progress Tracking

- Mark completed items with `[x]` immediately when done.
- Add newly discovered tasks with ➕ prefix.
- Document issues/blockers with ⚠️ prefix.
- Update plan if implementation deviates from original scope.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code changes, tests, lints — automatable in this repo.
- **Post-Completion** (no checkboxes): manual cleanup of `state.json` on the Pi, smoke-test of `/health` endpoint, release tag bump.

## Implementation Steps

### Task 1: Add `forget_script` and `is_orphan` helpers in `DaemonCore`

- [x] add `fn is_orphan(&self, name: &str) -> bool` in `daemon_core.rs` that returns `self.config.has_script_globally(name).is_none()`
- [x] add `async fn forget_script(&mut self, name: &str)` that: (1) calls `self.stop_script(name)` defensively (logs and ignores error), (2) calls `self.cron.cancel(name)`, (3) calls `self.state.remove_script(name)` (logs and ignores error), (4) removes the entry from `self.health` snapshot
- [x] write unit tests for `is_orphan`: empty config returns true; script in config returns false; script in another config (multi-config) returns false
- [x] write unit test for `forget_script`: pre-populate state + health with a script, call forget_script, assert both are empty afterwards
- [x] run `cargo test` and `cargo clippy --all-targets -- -D warnings` — must pass before Task 2

### Task 2: Reconcile orphans at end of `restore_state`

- [x] in `daemon_core.rs:restore_state`, after the config-loading loop (around line 730), collect `orphan_names: Vec<String> = self.state.scripts.iter().filter(|s| self.is_orphan(&s.name)).map(|s| s.name.clone()).collect()`
- [x] for each orphan, log `tracing::info!(script = %name, "Pruning orphan from state — not in any loaded config")` and call `self.forget_script(&name).await`
- [x] also skip inserting orphans into the health snapshot at lines 678-707 (filter before insert) — so they never appear in `/health` even briefly during startup
- [x] add unit/integration test `restore_state_prunes_orphans`: build a daemon with a temp `state.json` containing an entry whose name is not in the loaded `scripts.yml`; after `restore_state` returns, the script must be absent from both `state.scripts` and the health snapshot
- [x] add test `restore_state_keeps_scripts_in_other_config`: multi-config, script lives in `config_b` but state entry has `config_path = config_a`; assert it remains
- [x] add test `restore_state_drops_entries_for_missing_config_file`: state references a `config_path` that no longer exists on disk and the script is in no other config → script is pruned
- [x] run `cargo test` and `cargo clippy --all-targets -- -D warnings` — must pass before Task 3

### Task 3: Use `forget_script` in `reload_config` for removed scripts

- [x] in `daemon_core.rs:reload_config` (around line 872), replace the `for name in diff.removed { stop_script + state.remove_script }` block with `for name in diff.removed { if self.is_orphan(&name) { self.forget_script(&name).await; } else { self.stop_script(&name).await; } }`
- [x] rationale: a script removed from config A but still present in config B is not an orphan — only stop it; do not wipe state/health
- [x] add test `reload_removes_from_health`: load a config containing `foo`, simulate a process exit so `foo` ends up `Failed` in health, rewrite config without `foo`, call `reload_config`, assert `foo` is absent from health snapshot and state
- [x] add test `reload_keeps_script_present_in_other_config`: multi-config; remove script from config A while it still exists in config B; assert it is stopped (if running) but state/health entries remain
- [x] run `cargo test` and `cargo clippy --all-targets -- -D warnings` — must pass before Task 4

### Task 4: Verify acceptance criteria

- [x] verify all requirements from Overview are implemented (orphan in state at startup → pruned; orphan after reload → pruned; multi-config orphan rule respected)
- [x] run full `cargo test` — all tests green
- [x] run `cargo clippy --all-targets -- -D warnings` — zero warnings
- [x] run `cargo build --release` — must compile cleanly
- [x] confirm no public API surface change (binaries `th` and `turtled` keep the same flags and IPC commands)

### Task 5: [Final] Update documentation

- [ ] add a short note to `README.md` under a "State management" subsection (or extend "File Locations") explaining that scripts removed from config are pruned from state on next daemon start or on `Command::Reload`
- [ ] no CHANGELOG file in repo — skip
- [ ] bump `Cargo.toml` `version` to `0.6.2` (patch — bug fix, no API change)

*Note: ralphex automatically moves completed plans to `docs/plans/completed/`.*

## Technical Details

### Orphan rule

A script entry is orphan iff `ConfigManager::has_script_globally(name)` returns `None`. This handles:
- script deleted from its only config — orphan.
- script's `config_path` no longer exists on disk — config is not loaded → orphan.
- script moved to a different config (still loaded) — NOT orphan.

### `forget_script` semantics

```
forget_script(name):
  1. stop_script(name)        # defensive — script should not be running, but if it is, kill cleanly
  2. cron.cancel(name)        # remove any scheduled trigger
  3. state.remove_script(name) # persist to state.json (atomic via tmp+rename, see state.rs:60-69)
  4. health.write().remove(name) # drop from in-memory snapshot
```

All steps are idempotent — calling on an unknown name is safe.

### Why filter health insert in `restore_state`

`restore_state` inserts into health snapshot before loading configs. If we only prune after, there is a small window where `/health` could show the orphan as `Failed`. Easier to: load configs first, then build health snapshot using only non-orphan state entries. Order:
1. Load all `config_paths` from state into `ConfigManager` (existing logic at lines 710-728).
2. Build health snapshot from `state.scripts.iter().filter(|s| !self.is_orphan(&s.name))` (modified loop at 678-707).
3. Prune orphan entries from `state.scripts` and persist (`forget_script` for each).

This requires reordering the two existing blocks in `restore_state`.

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only.*

**Manual cleanup on the Pi (one-time, before first restart with the new binary)**:
- Optional: pre-clean `~/.local/share/turtle-harbor/state.json` with `jq 'del(.scripts[] | select(.name == "radio-t-checker" or .name == "radio-t-recorder"))'` to verify behaviour matches expectations. After deploy, this would be cleaned automatically.

**Manual smoke test after deploy**:
- `curl http://192.168.198.3:9200/health` should return HTTP 200 and only contain `radio-t-monitor` and `twitch-nfo-generator`.
- Gatus alerts for the turtle-harbor health endpoint should clear.

**Release**:
- Tag `v0.6.2` and let the existing release workflow (`.github/workflows/release.yaml`) build cross-platform binaries.
- Update Homebrew tap (`pkarpovich/homebrew-apps`) — handled by separate release tooling.

**Out of scope (deferred)**:
- 4 open dependabot PRs (#13, #14, #16, #17) and security alerts — separate session.
- `th prune` CLI command — not needed once startup + reload reconcile is in place. Reconsider only if a user complains.
- Periodic background reconcile — YAGNI.
