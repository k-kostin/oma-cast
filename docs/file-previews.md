# Selected-file previews

This change builds on the FFF file-search integration (PR #3). The preview layer
accepts a selected file path, not a search query; file ranking is unchanged.

## Processes and dependencies

The existing `oma-cast-files` binary has two independent modes:

- Default: the existing long-lived FFF index/search NDJSON service.
- `--preview /absolute/path`: one selected-file request and one JSON reply, then
  exit. It does not initialize FFF or acquire its index lock. Expensive rendering
  never runs on the search service or QML thread.

There is no new Python/Node runtime, linked renderer module or always-running
preview service. Rust/Cargo is needed when building the helper from source,
not when running a packaged binary. `md5` (freedesktop cache filenames) and
`crc32fast` (PNG integrity) are new Rust build dependencies. `url` and `tempfile`
already occur in the FFF lockfile and are now direct runtime dependencies;
all four are compiled into the helper, not separately installed by end users.

| File type | Preview | Optional external tool |
|---|---|---|
| PNG, JPEG, WebP, GIF, BMP | Existing Qt image decoder, EXIF orientation | None; format availability follows installed Qt codecs |
| Selected text formats | First 8 KiB, plain text | None |
| PDF | First page, longest edge 768px | Poppler `pdftoppm` |
| STL, OBJ, PLY | Transparent 512px thumbnail, fixed camera | F3D with native readers and software EGL |

External tools are optional and discovered on `PATH`. No automatic downloads,
package installation or privilege elevation. An unavailable tool produces a
preview message; searches and other formats remain usable. GIF previews use
Qt's still-image component, not an animation player. There is no PDF page
navigation, OCR, document-text search or interactive 3D viewer in this change.

F3D was validated on 4.0.0 Linux x86_64, Poppler on 26.08.0. Other distributions
must provide compatible CLI flags / EGL support; this is not a universal version
compatibility claim. F3D was chosen as a tested external renderer, not from an
exhaustive benchmark proving it optimal.

## Configuration and diagnosis

`~/.config/oma-cast/config.json` (the usual XDG override also applies):

```json
{
  "filePreviews": true,
  "previewF3d": "f3d",
  "previewPdfRenderer": "pdftoppm"
}
```

Executable values are a command name or an absolute path, treated as one argv
entry. They are never passed to a shell. For standalone helper invocation,
`OMA_CAST_F3D_BIN` and `OMA_CAST_PDFTOPPM_BIN` override configuration.

```sh
oma-cast files build             # rebuild older helpers; no separate preview build
oma-cast previews status         # no running shell required
oma-cast doctor                  # also diagnoses the launcher and FFF
```

The Bash CLI uses `jq`, already shipped in Omarchy's base packages and used
by the file-root setter, to read JSON. It does not invoke Python or depend on
the presence/version of the helper it diagnoses. Missing `jq` or an unreadable
status reply makes `doctor` fail explicitly, rather than report a healthy search.

Turning previews off cancels a pending request. It leaves the ordinary file
list and cached images intact. Uninstalling already removes the same helper
binary; there is no separate Python worker or downloaded renderer to clean up.

## Lifecycle, cache and limits

- 160ms selection debounce. One active preview per launcher instance. Selection,
  hide and unload cancel the current request; revision guards discard stale
  replies, including rapid A → B → A selection changes.
- Helper discovery follows the existing FFF candidates (explicit override,
  user installation, system installation). A failed process start tries the
  next candidate and eventually shows a rebuild message instead of loading forever.
- Valid standard freedesktop PNG thumbnails are read from the usual MD5-of-URI
  cache. URI, mtime, optional size and PNG checksums are verified. The helper's
  own generated PNGs live in `$XDG_CACHE_HOME/oma-cast/previews-v2/` and also
  validate nanosecond file timestamps. Writes use a private temporary file and
  atomic rename. Disabling previews does not purge caches. There is currently
  no total cache-size/age budget; to reclaim space, close the launcher and remove
  only `oma-cast/previews-v2/` beneath the configured cache directory (normally
  `~/.cache`). Generated thumbnails are recreated on demand; shared desktop
  thumbnails are left alone.
- The helper reads shared desktop thumbnails but does not yet publish new
  thumbnails there or execute arbitrary `.thumbnailer` definitions.
- File budget 128 MiB; STL budget 2 million triangles (binary / ASCII). Changed
  files are not committed to cache after a render. Unsupported types show a
  message. Standard image decoding runs asynchronously in Qt with a requested
  preview source size; it is not covered by the external-renderer address-space cap.
- F3D uses software EGL, two threads and nice +10. External renderers are bounded
  to 8 CPU seconds, 3 GiB address space, 16 MiB output, no core dumps and 10s
  worker wall time. QML has a 12s outer timeout and SIGTERM → SIGKILL escalation.
- Renderers run in a separate process group. Success, error, cancellation and
  timeout all kill remaining members before reaping the leader. The leader's
  unreaped PID pins group ownership until cleanup, including wrappers that exit
  before their children. Reaping is detached after cancellation so kernel-blocked
  renderer I/O cannot make the helper wait indefinitely.
- If the worker itself is abruptly killed with SIGKILL, Linux's parent-death
  signal kills its immediate renderer, **not arbitrary grandchildren**. Wrappers
  should `exec` the real renderer. Cleanup of grandchildren after worker SIGKILL,
  or processes that deliberately leave the group, is not guaranteed. Temporary
  outputs are private and disposable.

These are resource / crash isolation measures, **not a filesystem or network
security sandbox**. Remote mounts and kernel-uninterruptible filesystem reads
by the helper itself remain outside the local-filesystem acceptance evidence.
A future sandbox or shared-thumbnail-writer change needs its own review.

## Tests

```sh
node --test tests/*.test.js
cargo fmt --manifest-path helper/Cargo.toml -- --check
cargo test --locked --manifest-path helper/Cargo.toml
cargo clippy --locked --manifest-path helper/Cargo.toml --all-targets -- -D warnings
```

Rust integration tests launch the actual helper with tiny fixture files and
fake renderer executables; they do not require F3D/Poppler or Python. They
cover literal filenames, PNG cache integrity and freshness, shared-cache reads,
text limits, first-page PDF argv, missing renderers, bad input, file/triangle
budgets, ASCII STL whitespace, renderer descendants after success/error exit,
TERM and timeout, and the immediate renderer after worker SIGKILL. Existing FFF
protocol tests are retained. Private-HOME CLI tests check install/status/doctor/
uninstall, absent/older helpers, root decoding and missing jq, with a stub
graphical shell and no Python/Cargo/renderer executables on PATH. CI runs the
Rust tests, format and Clippy checks.

Real decoder/render/UI acceptance is a separate native Omarchy check; passing
unit tests alone does not establish live Qt codec, EGL or compositor support.
