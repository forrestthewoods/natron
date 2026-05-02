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
vs_channel   = "18"            # required; e.g. "17" for VS 2022
msvc_version = "14.50.18.0"    # optional; latest if omitted

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
- **`msvc`**: extract MSVC compiler + CRT from a Visual Studio channel
  manifest. Required `vs_channel`. Optional `msvc_version` (latest if
  omitted). Optional `manifest_history = true` (requires a pinned
  `msvc_version`): walks [`roblabla/msvc-manifest-history`][mh]'s
  `release-<vs_channel>` branch newest-first to find a historical
  snapshot containing the requested version. The only known workaround
  for older MSVC builds — Microsoft only serves the latest manifest.

  [mh]: https://github.com/roblabla/msvc-manifest-history
- **`windows_sdk`**: extract Windows SDK headers + libs from a VS channel
  manifest. Required `vs_channel`. Optional `sdk_version`.

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
