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
vs            = "vs2026"        # required; vs2019, vs2022, or vs2026
msvc_version = "14.51.36243"    # optional exact compiler package version
profile      = "standard"

[[toolchain]]
name        = "windows_sdk"
deploy_dir  = "windows_sdk"
provider    = "windows_sdk"
[toolchain.options]
vs_channel  = "18"
sdk_version = "26100"
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
- **`msvc`**: extract MSVC compiler, CRT, redistributable runtime, and
  optional native C++ feature packages from a Visual Studio channel
  manifest. Required `vs` (`vs2019`, `vs2022`, or `vs2026`). Optional
  `msvc_version`; if omitted,
  natron installs the latest MSVC toolset listed by Microsoft's live
  channel manifest. If pinned, `msvc_version` is the exact Visual Studio
  compiler package version, such as `14.51.36243`, and natron installs
  that compiler package version or fails. For older pinned versions no
  longer listed by Microsoft, natron also checks the unofficial
  [`roblabla/msvc-manifest-history`][mh] archive; pinned installs never
  silently fall back to latest.
- **`windows_sdk`**: extract Windows SDK headers + libs from a VS channel
  manifest. Required `vs_channel`. Optional `sdk_version`.

[mh]: https://github.com/roblabla/msvc-manifest-history

### MSVC package selection

MSVC's Visual Studio manifest contains hundreds of internal packages for
one toolset family. natron resolves the exact compiler package first, derives
that package family's `Microsoft.VC.<family>.` prefix, then selects real
manifest packages with simple glob patterns.

```toml
[toolchain.options]
vs           = "vs2026"
msvc_version = "14.52.36328"
profile      = "standard"
```

Profiles:

- `standard`: normal native C/C++ developer toolchain. Installs compiler
  tools for x64-host/x64-target, compiler resources, CRT headers, desktop +
  store CRT libs, CRT redist DLLs, and tiny declared resource / props /
  servicing metadata dependencies.
- `custom`: only the package patterns listed in `include`.
- `full`: every `Microsoft.VC.<resolved-family>.*` package in the exact
  resolved MSVC family. This is large; for MSVC `14.52` it is about 11 GiB
  deployed.

Standard with extras:

```toml
[toolchain.options]
vs           = "vs2026"
msvc_version = "14.52.36328"
profile      = "standard"

# Patterns without a Microsoft.* prefix match after stripping the resolved
# family prefix. For 14.52, "ATL.*.base" matches Microsoft.VC.14.52.ATL.*.base.
extras = [
  "ATL.*.base",
  "MFC.*.base",
  "ASAN.*.base",
]
```

Custom exact selection:

```toml
[toolchain.options]
vs           = "vs2026"
msvc_version = "14.52.36328"
profile      = "custom"

include = [
  "Tools.HostX64.TargetX64.base",
  "Tools.HostX64.TargetX64.Res*",
  "CRT.Headers.base",
  "CRT.x64.Desktop.base",
  "CRT.Redist.X64.base",
  "ATL.X64.base",
]
```

Full family mirror:

```toml
[toolchain.options]
vs           = "vs2026"
msvc_version = "14.52.36328"
profile      = "full"
```

Pattern rules:

- `*` matches any characters; `?` matches one character.
- Matching is case-insensitive.
- Patterns starting with `Microsoft.` match raw full package IDs for the
  resolved exact package version. This is an escape hatch for packages outside
  the resolved `Microsoft.VC.<family>.` prefix, such as
  `Microsoft.VC.Preview.DIA.*`.
- Every user-supplied pattern must match at least one package, otherwise the
  install fails instead of silently producing a partial toolchain.

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

```bash
cargo test                            # 160 tests, no network
NATRON_NETWORK_TESTS=1 cargo test     # adds tests/network.rs (real upstream)
```

## License

MIT OR Unlicense.
