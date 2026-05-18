# rojo-push — fork notes

This is a fork of [rojo-rbx/rojo](https://github.com/rojo-rbx/rojo) that adds a
manual-push sync mode. It exists because Rojo's filesystem watcher is unreliable
when the project lives behind a Windows directory junction (events don't
propagate), and the same problem affects network shares and some cross-directory
project layouts. Upstream Rojo's default behavior is unchanged; everything new
is opt-in.

## Workflow

Start a serve session with the watcher disabled. The tree is built once at
startup; nothing else updates it automatically.

```
rojo serve --no-watch
```

After every build of the upstream library (or any disk change you want
reflected in Studio), run:

```
rojo push
```

That's it. `rojo push` POSTs to `/api/refresh` on the running serve instance,
which re-snapshots the project from disk, computes a patch against the
in-memory tree, and pushes the patch to all connected Studio plugins via the
existing message queue. The plugin needs no changes — refresh-triggered patches
look identical to watcher-triggered ones from its perspective.

### Typical Claude Code loop

```bash
# In one terminal — leave running:
rojo serve --no-watch

# In Claude Code — after every library build:
bun run build && rojo push
```

No watchers, no restarts, no missed events.

## HTTP API

### `POST /api/refresh`

Body: none required.

Response (JSON, always — unlike the msgpack `/api/*` plugin routes):

```json
{
  "sessionId": "f4f7cc9d-c7c4-421a-82d1-7afa89b9a3f9",
  "instancesAdded": 1,
  "instancesRemoved": 0,
  "instancesUpdated": 2,
  "durationMs": 3,
  "errors": []
}
```

Status codes:
- `200 OK` — patch applied, no errors
- `207 Multi-Status` — patch applied, but at least one snapshot error was raised
  (look at `errors` for details; the patch is still authoritative for what
  actually changed in the tree)
- `500 Internal Server Error` — the refresh could not run at all

### `GET /api/rojo` (extended)

Now includes `watchEnabled: bool` so tools can detect whether the server is
running in push mode. Omitted when `true` for back-compat (older clients
treat its absence as `true`).

## CLI

### `rojo serve [--no-watch]`

`--no-watch` disables the filesystem watcher. The project is snapshotted once
at startup. Subsequent updates only happen via `POST /api/refresh` or
`rojo push`.

### `rojo push [--address ADDR] [--port PORT] [--timeout SECS]`

POSTs `/api/refresh` and prints a one-line summary like:

```
Pushed: +1 ~2 -0 (3 ms)
```

Exits non-zero if the server reports errors in the response body (you'll see
the error list printed).

## Architecture (for anyone hacking on this)

The change in shape, not in volume:

1. `memofs::Vfs::set_watch_enabled(false)` already existed upstream. With
   `--no-watch`, `ServeSession::new_with_options` calls it before any reads, so
   no inotify/FSEvents/ReadDirectoryChangesW handles are registered.
2. `ChangeProcessor` (the sole thread that writes to the `RojoTree`) gained a
   fourth `select!` arm: `refresh_request_receiver`. When a request arrives,
   it calls the existing `compute_and_apply_changes(tree, vfs, root_id)`
   helper — same code path that file events take. The result is pushed onto
   `MessageQueue` like any other patch.
3. The HTTP handler at `POST /api/refresh` (in `src/web/api.rs`) calls
   `ServeSession::refresh()`, which sends a request on the new channel and
   awaits a oneshot reply. The handler runs the blocking call on tokio's
   blocking pool to keep the runtime healthy.
4. The Studio plugin reads patches off the same message queue via
   `/api/socket/{cursor}` long-poll, so refresh-triggered patches flow through
   the existing plugin protocol without any plugin-side changes.

## Pulling from upstream

The upstream remote is kept as `upstream`:

```
git remote -v
# upstream  https://github.com/rojo-rbx/rojo.git (fetch)
# upstream  https://github.com/rojo-rbx/rojo.git (push)
# origin    https://github.com/StephenSHorton/rojo-push.git (fetch)
# origin    https://github.com/StephenSHorton/rojo-push.git (push)
```

To pull upstream changes:

```
git fetch upstream
git rebase upstream/master   # or git merge upstream/master
```

The fork's changes are concentrated in:

- `src/cli/serve.rs` — `--no-watch` flag
- `src/cli/push.rs` — new `rojo push` subcommand
- `src/cli/mod.rs` — subcommand registration
- `src/serve_session.rs` — `ServeSessionOptions`, `refresh()` method
- `src/change_processor.rs` — refresh channel, `handle_refresh_request`
- `src/web/api.rs` — `POST /api/refresh` handler
- `src/web/interface.rs` — `RefreshResponse`, `ServerInfoResponse.watchEnabled`

If upstream touches any of those (especially `change_processor.rs` or
`serve_session.rs`), expect a merge conflict and review carefully. The rest of
the codebase is untouched.

## Known quirks

- **`instancesUpdated` counts only content-changing updates.** When the refresh
  re-snapshots the entire root, the patch_compute path produces metadata-only
  diffs for many instances even when their content is unchanged — most likely
  because `InstanceMetadata` contains a `Vec<PathBuf>` (`relevant_paths`) whose
  order isn't stable across snapshots, or an `InstigatingSource::ProjectNode`
  embedding a nested `ProjectNode` that isn't stably equal. These no-op patches
  still flow to the Studio plugin through the message queue (consistent with
  the upstream watcher path), but the summary excludes them so the count
  reflects what the user actually changed on disk. If you ever see
  `instancesUpdated > 0` after a refresh you didn't expect, the build wrote
  files you weren't expecting — not a false positive. Tracking this as a
  potential upstream fix (TODO).

## What this fork does NOT change

- The binary is still named `rojo`. Existing Rokit configs, VS Code Rojo
  plugin, etc. keep working.
- Default `rojo serve` (no `--no-watch`) behaves exactly as upstream.
- The plugin and its protocol are unchanged.
- All upstream tests still pass.
