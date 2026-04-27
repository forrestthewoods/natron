# Ideas — possible future features

A parking lot for things deliberately deferred from v1.

## CLI

- **`natron dedupe`** — manual CAS rescue pass for caches built without
  CAS (e.g., `--no-cas` runs followed by a switch back) or that drifted
  somehow. Walk `<cache>/installs/`, hash files, link duplicates through
  CAS. Idempotent.
- **`natron verify`** — recompute hashes for `copy` mode deploy trees,
  check link integrity for `symlink`/`hardlink` modes, validate
  `metadata.toml` schema.
- **`natron add <provider> ...`** — append a `[[toolchain]]` to the
  config file from the CLI, with `--install` to also run sync.
- **`natron which <name>`** — print the deployed path for shell
  consumption: `cc=$(natron which llvm21)/bin/clang`.
- **`natron info <name>`** — show `metadata.toml` + deploy state for one
  toolchain.
- **`natron update` / `natron upgrade`** — bump a toolchain to its
  upstream-latest version; for github maybe surface available tags.
- **`natron clean --orphans`** — remove cache installs not referenced by
  any known project. Needs a global registry of known projects to be
  safe; deferred.
- **`natron clean --cas`** — GC CAS blobs whose `nlink == 1` (only the
  CAS itself references them).
- **`natron clean --staging`** — manual staging GC; today the install
  flow does this automatically with a 60-min threshold.

## Reliability / performance

- HTTP retries with exponential backoff for flaky downloads. LLVM is
  ~1.5 GB and a transient failure means the whole download restarts.
- Parallel CAS hash pass (rayon) — meaningful for MSVC's hundreds of
  files.
- Parallel cross-toolchain installs.
- Rate-limit handling for the GitHub releases API; an env-var auth
  token for the github provider.

## Provider features

- **Plugin providers** loaded from out-of-tree crates. The `Provider`
  trait is already `dyn`-safe, so this is a feature gate + dynamic load
  pattern away. The library API exposes `ProviderRegistry::register`
  for in-process registration; out-of-process plugins would need a
  loader.
- **Sidecar `.sha256` lookup** for the github provider (when the upstream
  publishes a `<asset>.sha256` next to the release asset).
- **Auto-detection of `platform` / `arch`** when the user doesn't
  specify (today, `zig`'s `platform` is required).
- **`tar.gz`** support — the parser stub currently errors with a clear
  message. Add `flate2` and wire it up if a real consumer appears.
- **VSIX / MSI** as user-visible `archive` kinds, in case someone wants
  the `url` provider to ship a Microsoft-flavored archive.

## Schema / state

- **Lockfile generation** — only if a real need emerges. The plan
  explicitly chose "config IS the lock"; lockfiles add an extra moving
  part.
- **Schema migration** for `metadata.toml` and `.natron-state.toml`
  beyond the v1 hard-error.
- **`info` output as JSON** for tooling integration.

## Test infrastructure

- **Hash-collision fault injection**: a test-only hasher that returns
  the same digest for two distinct inputs, exercising the byte-compare
  path in `cas.rs`. Today that branch is logically tested via inspection
  but not executed.
- **Multi-process concurrency test** — `tests/offline.rs::test_concurrent_install`
  uses two threads (same process). A true multi-process variant would
  spawn the natron binary as a subprocess twice; harness cost is real
  but the coverage is stronger.

## Build-system integration

Out of scope for natron itself, but the library API is shaped so
consumers (Anubis, etc.) can build on top:

- A "make-vars" mode that emits Make-readable variables pointing at
  deployed paths.
- A `cargo` integration that points at toolchains for cross-compiles.
- A CMake `find_package`-style module.
