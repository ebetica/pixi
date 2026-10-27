# Shared-cache permission patches

This fork lets several users share one pixi cache directory (`PIXI_CACHE_DIR`).

Upstream pixi writes parts of its cache through `tempfile`, which always creates
files with mode `0600`. Users who did not write a file then cannot read it, and
`pixi add`/`pixi update`/fresh installs fail with
`failed to read from file .../repodata/shards-v1/<hash>.msgpack: Permission denied`.
The fix creates those files with mode `0666`, so the umask (or a default ACL on
the cache directory) decides who can read them. Other files and directories
already followed the umask.

What is here:

- `rattler.diff`: the source changes from conda/rattler#2950 (not yet merged
  upstream) for repodata, shard, run-exports, virtual-package and prefix-record
  files.
- `cacache.diff`: the same change for `cacache`, which pixi uses for its
  conda-to-PyPI name mapping cache.
- `rattler_shared_tempfile/`: the small helper crate that the diffs use.
- `shared-group.diff`: the `[cache] shared-group` setting (below), for
  `fs-err`, `tempfile`, `uv-build-frontend`, rattler's package cache and
  repodata locks, and `cacache`. They call `shared_cache_group/`.
- Every other directory is a crates.io crate at the version in `Cargo.lock`,
  with the diffs applied. `scripts/vendor_shared_cache.py` regenerates them
  and the marked block in `Cargo.toml`'s `[patch.crates-io]`.

## Sharing the cache with a group

With only the fix above, the cache needs a directory owned by a shared group
with the setgid bit (`chgrp pixi-users $DIR && chmod 2775 $DIR`) and every user
running with `umask 002` (or a default ACL on the directory). Otherwise the
first user's lock files and directories are not group-writable and everyone
after them fails with `failed to acquire global cache lock` or
`no writable layers to cache package to`.

Setting a group in the global config (`~/.pixi/config.toml` or the system
config) removes both requirements:

```toml
[cache]
root = "/tmp/pixi-cache"
shared-group = "slurm-users"   # or PIXI_CACHE_SHARED_GROUP=slurm-users
```

Then every directory pixi, rattler or uv creates in the cache, the root
included, is owned by that group with mode `2775`, and every file (lock files
included) is group read/write, whatever the umask. Nothing needs to create the
root beforehand. Directories and lock files appear under a temporary name and
are renamed into place without replacing anything, so two users creating the
same one at the same time both succeed. Trees written by code that bypasses
this (conda package extraction, build backends writing into an unpacked
source distribution) are fixed up before they are used. The root and any
`[cache.<kind>]`/`PIXI_CACHE_<KIND>_DIR` directories are shared; detached
environments are not. If the group does not exist or you are not a member,
pixi warns and uses the cache as without the setting. Entries other users
created are never changed. See `shared_cache_group/src/lib.rs`.

## Installing

```bash
curl -fsSL https://github.com/ebetica/pixi/releases/latest/download/install.sh | sh
# point the user at a shared cache too (adds `export PIXI_CACHE_DIR=...` to the shell rc):
curl -fsSL https://github.com/ebetica/pixi/releases/latest/download/install.sh \
  | PIXI_SHARED_CACHE_DIR=/shared/pixi-cache sh
# a specific version (the v0.83.0.biohub release):
curl -fsSL https://github.com/ebetica/pixi/releases/download/v0.83.0.biohub/install.sh | PIXI_VERSION=0.83.0 sh
```

The installer is `install/install-shared-cache.sh`. It installs into `~/.pixi/bin`,
replacing an upstream pixi there. It checks the release checksum and warns if the
cache directory or your umask will not work for sharing. Only Linux x86_64
builds exist so far.

## Switching versions

When a workspace's `requires-pixi` is not satisfied, this pixi looks for the
newest release of this fork tagged `v<version>.biohub` whose version satisfies
it, installs its binary into `$PIXI_HOME/versions/<version>/pixi` (checksum
verified, renamed into place) and re-runs itself with that binary and the same
arguments and environment. A matching version that is already installed is used
without network access. Only releases of this fork are used, never upstream
pixi; without a matching fork release the usual `requires-pixi` error is shown,
with the versions that are available. See `crates/pixi_cli/src/auto_switch.rs`.

- `PIXI_NO_AUTO_SWITCH=1` turns this off.
- `PIXI_AUTO_SWITCHED_EXE` is set on the re-run process, so a binary that still
  does not satisfy the requirement fails instead of switching again.
- Releases are listed through the GitHub API, which allows 60 unauthenticated
  requests per hour per IP; set `GITHUB_TOKEN` if that is not enough.

## Releasing

Tag the rebased branch `v<version>.biohub` (the binary itself reports plain
`<version>`), then attach these release assets:
`pixi-x86_64-unknown-linux-musl.tar.gz`, its `.sha256`, and
`install/install-shared-cache.sh` renamed to `install.sh`. Mark the release
as latest unless it is an older pixi version published for pinned workspaces.

## Rebasing onto a new pixi release

```bash
git rebase --onto v<new> v<old> shared-cache/v<old>   # conflicts are usually only Cargo.toml/Cargo.lock
python scripts/vendor_shared_cache.py                 # re-vendors at the new locked versions
cargo metadata --format-version=1 >/dev/null          # refreshes Cargo.lock
```

If a diff no longer applies, regenerate it against the new crate versions. Drop
the rattler part once conda/rattler#2950 lands in a release pixi uses.

## Building a release binary

Build it like upstream does (`scripts/build_pgo.py`), but without the
`self_update` feature, so `pixi self-update` cannot replace the patched binary
with an upstream release:

```bash
CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=symbols \
PIXI_SELF_UPDATE_DISABLED_MESSAGE="This pixi build carries the shared-cache patches; install the matching patched build instead." \
cargo zigbuild --locked --release --target x86_64-unknown-linux-musl --bin pixi --features performance \
  --manifest-path crates/pixi/Cargo.toml
```
