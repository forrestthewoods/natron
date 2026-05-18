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
msvc_version = "14.51.36243"    # optional exact compiler package version
profile      = "standard"
hosts        = ["x64"]
targets      = ["x64"]
locales      = ["en-US"]

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
  manifest. Required `vs_channel`. Optional `msvc_version`; if omitted,
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
one toolset family. natron exposes stable developer-level options instead
of requiring raw Microsoft package IDs.

```toml
[toolchain.options]
vs_channel   = "18"
msvc_version = "14.52.36328"

profile = "standard"
hosts   = ["x64"]
targets = ["x64"]
locales = ["en-US"]
```

Profiles:

- `standard`: normal native C/C++ developer toolchain. Installs compiler
  tools for each host/target pair, selected compiler resources, CRT
  headers, desktop + store CRT libs, CRT redist DLLs, and tiny declared
  `Props.*` / `Servicing.*` metadata dependencies.
- `custom`: same host/target/locales model, but `crt_libs`, `runtimes`,
  and `features` are explicit.
- `full`: every `Microsoft.VC.<resolved-family>.*` package in the exact
  resolved MSVC family. This is large; for MSVC `14.52` it is about 11 GiB
  deployed.

Custom selection example:

```toml
[toolchain.options]
vs_channel   = "18"
msvc_version = "14.52.36328"

profile = "custom"
hosts   = ["x64", "arm64"]
targets = ["x64", "arm64"]
locales = ["en-US"]

crt_libs = ["desktop", "store"]
runtimes = ["crt"]
features = ["atl", "mfc", "asan", "pgo", "code_analysis"]
```

Full family mirror:

```toml
[toolchain.options]
vs_channel   = "18"
msvc_version = "14.52.36328"
profile      = "full"
```

Supported values:

- `hosts`: `x64`, `x86`, `arm64`.
- `targets`: `x64`, `x86`, `arm64`.
- `locales`: concrete VS locales like `en-US`, or `["all"]`.
- `crt_libs`: `desktop`, `store`, `onecore`, `spectre`, `debug`.
- `runtimes`: `crt`, `crt_spectre`, `mfc`, `mfc_spectre`.
- `features`: `atl`, `atl_spectre`, `mfc`, `mfc_spectre`, `mfc_mbcs`,
  `asan`, `pgo`, `cli`, `code_analysis`, `dia_sdk`, `source`.

Feature notes:

- `atl`: Active Template Library support, mostly for COM-heavy Windows C++.
- `mfc`: Microsoft Foundation Classes, the classic Win32 C++ app framework.
- `asan`: AddressSanitizer runtime/support for memory bug detection.
- `pgo`: profile-guided optimization tools and support files, including
  Microsoft packages named `Premium.Tools.*` internally.
- `cli`: C++/CLI support for `/clr` native/.NET interop, not command-line
  tools.
- `code_analysis`: MSVC `/analyze` static analysis engine and rulesets.
- `dia_sdk`: Debug Interface Access SDK.
- `source`: Microsoft source payloads for CRT/ATL/MFC/CLI when available.

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
