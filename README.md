# natron

Vendor compiler toolchains into source-controlled projects.

natron is a small Rust library + CLI that downloads, caches, and deploys
compiler toolchains (LLVM, Zig, NASM, MSVC, Windows SDK, etc.) declared in
a TOML config file. Toolchains live in a shared content-addressed cache;
each project gets a deploy directory containing hardlinks (or symlinks, or
copies) into the cache.

The name comes from the Egyptian preservation salt — natron preserves
toolchains the way mummification preserved bodies, with rather better
ergonomics.

## Why

If your build depends on `clang-21.1.6` for Windows MSVC builds, you don't
want to depend on whatever happens to be on `$PATH`. natron pins exact
versions in `natron.toml`, fetches them from upstream, and stages them
under `<project>/toolchains/` so the build sees a stable, named tree.

## Usage

```bash
natron install         # install + deploy everything in natron.toml
natron install --dry-run
natron install --only llvm21
natron install --mode copy   # override deploy mode for this run
natron list                  # show what's deployed in this project
natron list --cache          # show every install in the global cache
natron clean --downloads     # empty <cache>/downloads/
natron clean --all --yes     # nuke the whole cache
natron msvc versions         # list Microsoft VS builds available on the mirror
natron msvc packages --build-version 18.6.11819.183
natron msvc extract  --build-version 18.6.11819.183 --out C:\temp\msvc
natron windows_sdk versions  # list Windows SDK versions available on the mirror
natron windows_sdk packages --sdk-version 26100
natron windows_sdk extract  --sdk-version 26100 --out C:\temp\sdk
```

`natron` with no subcommand defaults to `install`.

## Config

A `natron.toml` at your project root:

```toml
[settings]
deploy_dir   = "toolchains"
deploy_mode  = "hardlink"     # default; per-toolchain may override
cache_dir    = "~/.cache/natron"  # optional

[[toolchain]]
name        = "llvm21"
deploy_dir  = "llvm21"
provider    = "github"
[toolchain.options]
repo  = "llvm/llvm-project"
tag   = "llvmorg-21.1.6"
asset = "clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz"
strip_prefix = "clang+llvm-21.1.6-x86_64-pc-windows-msvc"

[[toolchain]]
name        = "nasm"
deploy_dir  = "nasm"
provider    = "url"
[toolchain.options]
url     = "https://www.nasm.us/pub/nasm/releasebuilds/3.01/win64/nasm-3.01-win64.zip"
strip_prefix = "nasm-3.01"

[[toolchain]]
name        = "zig"
deploy_dir  = "zig"
provider    = "zig"
[toolchain.options]
version  = "0.15.2"
platform = "x86_64-windows"

[[toolchain]]
name        = "msvc"
deploy_dir  = "msvc"
provider    = "msvc"
[toolchain.options]
build_version = "18.6.11819.183"   # required: exact Microsoft VS build (see `natron msvc versions`)

[[toolchain]]
name        = "windows_sdk"
deploy_dir  = "windows_sdk"
provider    = "windows_sdk"
[toolchain.options]
sdk_version = "26100"              # required: exact Windows SDK build (see `natron windows_sdk versions`)
```

Multiple toolchains of the same provider type are first-class. Vendor
LLVM 18 and LLVM 21 side by side; each gets its own `[[toolchain]]`
block with distinct `name` and `deploy_dir`.

## Built-in providers

- **`url`**: download a fixed URL (http/https/file). Required `url`. Optional
  `sha256`, `archive` (inferred from URL filename), `strip_prefix`.
- **`github`**: download a GitHub release asset. Required `repo`, `tag`,
  `asset`. Optional `version` (display), `sha256`, `archive`, `strip_prefix`.
- **`zig`**: look up `version` + `platform` in
  `https://ziglang.org/download/index.json`, sha-verified via the index.
- **`msvc`**: install MSVC compiler + CRT + redist from one exact
  Microsoft VS build. Required `build_version` (e.g.
  `18.6.11819.183`); its major (16/17/18) selects the VS series.
  Optional `base_install` (`none` | `default` | `full`, default
  `default`) and `extras` (list of glob patterns added on top of the
  base set). See "MSVC package selection" below.
- **`windows_sdk`**: install Windows SDK headers + libs. Required
  `sdk_version` (e.g. `26100`). Optional `base_install` (`none` |
  `default` | `full`, default `default`) and `extras` (list of MSI
  filename prefixes added on top). Independently versioned from MSVC.
  See "Windows SDK package selection" below.

Both providers source data from the
[`roblabla/msvc-manifest-history`][mh] community mirror — Microsoft's
per-VS-release `channel.json` and `manifest.json`, snapshotted on
`release-{16,17,18}` branches. MSVC pins one snapshot via `build_version`
(the only string Microsoft guarantees identifies a specific installer
build, so the only string that gives 100%-reproducible MSVC installs).
The Windows SDK is independently versioned by Microsoft and ships
immutable payloads per `sdk_version`, so it pins that directly without
caring which snapshot the manifest came from.

If the mirror goes away, both installs break. Known tradeoff —
preferable to losing historical-version reproducibility.

[mh]: https://github.com/roblabla/msvc-manifest-history

### MSVC package selection

`base_install` chooses the starting set; `extras` adds more on top.

- `default` (the default): compiler + locale resources + CRT headers +
  desktop CRT + store CRT + redist. Roughly the minimum useful native
  C/C++ toolchain.
- `full`: every package in the snapshot (including legacy compat
  toolsets Microsoft re-ships in every VS installer). ~10 GB.
  Mutually exclusive with `extras`.
- `none`: install only what's in `extras`. Requires at least one
  pattern.

```toml
# Just the default set.
[toolchain.options]
build_version = "18.6.11819.183"

# Default + a few extras.
[toolchain.options]
build_version = "18.6.11819.183"
extras        = ["ATL.*", "MFC.*"]

# Everything in the snapshot.
[toolchain.options]
build_version = "18.6.11819.183"
base_install  = "full"
```

Pattern rules for `extras`:

- `*` matches any characters; `?` matches one character.
- Matching is case-insensitive.
- Patterns without a `Microsoft.` prefix match against the
  family-relative tail. For a snapshot whose primary compiler family is
  `Microsoft.VC.14.50.18.0`, `ATL.X64.base` matches
  `Microsoft.VC.14.50.18.0.ATL.X64.base`.
- Patterns starting with `Microsoft.` match raw package IDs (escape
  hatch for outside-family packages like
  `Microsoft.VC.Preview.DIA.*`).
- Every pattern must match at least one package, otherwise the install
  fails instead of silently producing a partial toolchain.

### MSVC debug commands

```bash
# Every available Microsoft VS build per series (newest-first).
natron msvc versions
natron msvc versions --vs vs2026

# Every package in one build's snapshot, grouped: family first, other.
natron msvc packages --build-version 18.6.11819.183

# Download + extract every package at a build into per-package dirs.
# Use this to discover which package contains a missing file (grep,
# Explorer, Everything, etc.). You manage cleanup of the --out directory.
natron msvc extract --build-version 18.6.11819.183 --out C:\temp\msvc
```

`versions` enumerates each release branch's commits via the GitHub API
and fetches each commit's small `channel.json` (~130 KB; cached after
first run). `extract` reuses the global download cache, so re-runs don't
re-download. Already-populated package dirs in `--out` are skipped.

### Windows SDK package selection

Same `base_install` shape as MSVC, but the selection unit is **MSI
filename prefix** rather than package id glob — the SDK's user-facing
"components" are at the MSI level.

- `default` (the default): 7 essential MSIs that get you a typical
  C/C++ dev set — Universal CRT, Win32 desktop headers/libs, OneCore
  headers, UWP/Windows Store headers + libs + tools.
- `full`: every MSI in the SDK component meta-package's dep graph
  (debuggers, ARM/ARM64 target libs, driver headers, signing tools,
  etc.). ~3-4 GB. Mutually exclusive with `extras`.
- `none`: install only what's in `extras`. Requires at least one prefix.

```toml
# Default install.
[toolchain.options]
sdk_version = "26100"

# Default + extras (common: signtool, mt.exe, rc.exe, ARM64 libs).
[toolchain.options]
sdk_version = "26100"
extras = [
  "Universal CRT Tools x64",         # signtool, etc.
  "SDK ARM64 Additions",
]

# Everything.
[toolchain.options]
sdk_version  = "26100"
base_install = "full"
```

`extras` entries are plain filename prefixes — no globs. Each entry must
match at least one MSI in the SDK, otherwise install fails (no silent
typos).

### Windows SDK debug commands

```bash
# Every available Windows SDK version (newest-first).
natron windows_sdk versions

# Every MSI in a specific SDK, grouped by default-installed vs
# available-for-extras. Use this to discover what to put in extras.
natron windows_sdk packages --sdk-version 26100

# Download + extract every MSI from one SDK into per-MSI dirs.
# Use this to discover which MSI contains a missing file.
natron windows_sdk extract --sdk-version 26100 --out C:\temp\sdk
```

## Deploy modes

- **`symlink`** (default): a single directory symlink from the deploy dir
  to the cache install tree. Instant. Atomic version swaps (just rewrite
  the link). Cross-volume safe. On Windows, falls back to a directory
  junction when creating symlinks lacks privilege.
- **`hardlink`**: mirror the cache tree with one hardlink per file. The
  deploy looks like a plain directory tree to every tool. Requires the
  deploy dir on the same filesystem volume as the cache. Use this when
  a tool you depend on can't follow reparse points (rare).
- **`copy`**: a real file copy. Slow and uses real disk space, but the
  only mode where deployed files are independent of the cache — use
  when you want to commit the toolchain into source control.

The cache itself always uses hardlinks internally (each install tree's
files are hardlinks into a content-addressed `cas/` directory). That
machinery is independent of the deploy mode and isn't user-visible.

## Cache layout

natron uses one global cache (default `~/.natron/` on every platform —
shared across all projects, like `~/.cargo` or `~/.rustup`):

```
<cache>/
  installs/
    <fingerprint>/
      tree/                  # the install (read-only)
      metadata.toml
  cas/<aa>/<bbcc.../>        # content-addressed blobs (xxhash3-128)
  downloads/                 # archive download cache
  staging/                   # in-progress installs (renamed atomically into installs/)
```

The CAS dedupes byte-identical files across toolchains. Two LLVM versions
that share a `LICENSE` or a sub-component end up sharing one inode in the
cache, with hardlinks into each install tree.

## Library API

```rust
use natron::{Natron, Config};

let n = Natron::from_config_file("natron.toml".as_ref())?;
let report = n.sync()?;
for entry in &report.entries {
    println!("{}: {:?}", entry.name, entry.action);
}
```

For tests / advanced consumers, `Natron::from_config_with_registry(cfg, reg)`
takes a custom `ProviderRegistry`, letting you point providers at fixture
URLs (every network-using provider has a `with_*_url` constructor for
overriding its base URL).

## Tests

`cargo test` is fully hermetic — no network. Synthetic archives and fake
release JSON are generated at test time and served via `file://` URLs.

Integration tests that hit real upstream services are individually
`#[ignore]`'d. Use cargo's standard runner flags to opt in:

```bash
cargo test                              # hermetic only (default)
cargo test -- --ignored                 # only the network tests
cargo test -- --include-ignored         # both
```

## License

MIT OR Unlicense.
