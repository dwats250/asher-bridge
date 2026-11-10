# Toolchain and offline build environment

The pinned Rust toolchain and dependency cache for Asher Bridge, and the one way to use them. Implemented by `scripts/ab-env.sh` (checks), `scripts/ab-sandbox.sh` (the isolation), `scripts/ab-verify.sh` (the reproduction) and `scripts/ab-selftest.sh` (negative tests of the environment itself).

## Pin

| Item | Value |
|---|---|
| rustc / rustdoc | `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, LLVM 23.1.1, host `x86_64-unknown-linux-gnu` |
| cargo | `cargo 1.99.1 (5f94df478 2026-08-27)` |
| rustfmt | `rustfmt 1.10.0-stable (58cf8f9c35 2026-10-13)` |
| clippy | `clippy 0.1.99 (58cf8f9c35 2026-10-13)` |
| SHA-256 `rustc` | `b53b780f77f952062f8ca8a3b70aef2390b1e45ad91939be94afdc9543026853` |
| SHA-256 `cargo` | `a670e02cfa39ac2e53f77a2d911ca1fc9e2c479b0603ec956625be1daf24e3af` |
| SHA-256 `cargo-clippy` | `4255f7671a17fba900ffd84a20f1dd0c986d52a2404496d3b7518f5044de5663` |
| SHA-256 `clippy-driver` | `89b6cc8d0d988b00a3f65bc90c76fedbaedb6bcb442b34563aefe3621f26544b` |
| SHA-256 `rustfmt` | `68cb47a2a38daa8ff2ec6d623a2374b0a165d77ec74f1f654216665018c04598` |
| SHA-256 `rustdoc` | `832e53ca262db3d3fcaca659dc6e71f0f5a924a48adc580ee4278acac75f3e80` |
| SHA-256 `cargo-fmt` | `6b42f43e4523abba428eb42b3a106727843428b096ef7ed8a9497bddcc50448a` |
| Toolchain tree digest | `906ed6e9af1f2c119863d71586ef07dc3667d5d08034566fd9c7e1b3fc467c6f`: SHA-256 of the sorted `sha256sum` of all 166 regular files under the toolchain directory (paths relative to it) followed by the sorted symlink list (there are none) |
| SHA-256 `multirust-channel-manifest.toml` | `48419307eb32e389a25ac360b6bccd4d196f3c4f35ce9d67a223319eff017c6c` |
| Manifest floor | `rust-version = "1.85"`, edition 2024 (1.99.1 satisfies it) |
| `Cargo.lock` | version 4, 67 packages, SHA-256 `f7751a1bd52ab4ce528700c558f878476dcd6e5cfa31707dc2cc2cad218f33e0` |
| Native requirements | a C compiler and linker (tested with `cc` 14.2.0: `rusqlite` builds bundled SQLite), `git`, `/bin/sleep`, `/bin/true`, `strings` |

`ab-env.sh` checks the five version lines (rustc, cargo, rustfmt, clippy, rustdoc), the seven binary digests above, the digest of the whole toolchain tree (so the compiler proper, the standard library and every other file are pinned by content, not by version string), the `Cargo.lock` digest, the code pin, and that no tracked, untracked or ignored change exists in the code paths. The `multirust-channel-manifest.toml` digest is covered by the tree digest.

The code pin is `pin_code` in `scripts/ab-env.sh`: a SHA-256 over the sorted list of per-file SHA-256 digests of `src/`, `tests/`, `migrations/`, `Cargo.toml` and `Cargo.lock`. It is computed from file content, so it does not depend on repository history. A Git checkout is still required, because the clean-tree check uses `git status`. Whoever changes a file in those paths must update `pin_code`, and the expected test count in `scripts/ab-verify.sh` if the number of tests changes; until then the scripts refuse to build.

## What `AB_RUST_ROOT` must contain

The repository ships no toolchain and no dependency sources, and the scripts download nothing. `AB_RUST_ROOT` is **required** (there is no default location) and must name a directory holding:

```
$AB_RUST_ROOT/rustup/toolchains/stable-x86_64-unknown-linux-gnu/   (about 617 MB; rustup profile minimal + clippy + rustfmt)
$AB_RUST_ROOT/cargo/registry/                                      (about 12 MB cache, 68 MB extracted sources, 6 MB index cache)
```

`AB_TOOLCHAIN_BIN` and `AB_REGISTRY_SEED` may override the two derived paths (the toolchain's `bin/` directory and the registry seed). Both trees are used in place and read-only; the scripts never move, modify or clean them.

**Obtaining the pinned Rust 1.99.1 toolchain and the dependency registry is your responsibility.** The expected source is the standard rustup distribution of that version for `x86_64-unknown-linux-gnu` (profile minimal plus the `clippy` and `rustfmt` components), and a Cargo registry populated for this `Cargo.lock` (see "Registry seed"). Whether the files of such an installation on another machine match the digests pinned above has **not** been verified. If they differ, `ab-env.sh` refuses to run and names the difference; the pin is not relaxed automatically. After copying or relocating the trees, only `AB_RUST_ROOT` changes, and every digest is checked again on each run.

## Why rustup is not used

The scripts put the toolchain's own `bin/` directory first on `PATH` by absolute path and never run the `rustup` proxy. The proxy can write into its home (`tmp`, `update-hashes`) and would try to download a toolchain if a pin file named a channel the installation does not have. That is why the repository has **no** `rust-toolchain.toml` and **no** `.cargo/config.toml`: either would alter every future build and, under rustup, would need the network. `ab-env.sh` refuses to run if either (or `build.rs`) appears anywhere in the repository, tracked or not, or if `CARGO_HOME` holds a `config`/`config.toml`.

## Environment contract

For build id `<id>`: lowercase `[a-z0-9-]`, not starting with `-`, **at most 6 characters**, and **mandatory** (`AB_ID` has no default; an unset or invalid value stops everything before anything is written).

### Build root identity

The build root is `/var/tmp/ab-<id>/` with `tmp/ target/ cargo/ home/`, mode 700, and a `.ab-root` marker (id, uid, repository, HEAD, creation time) written by the run that created it.

- `scripts/ab-verify.sh` reserves the root with `mkdir -m 700` **without `-p`**, which is atomic and fails on any existing file, directory or symlink (dangling or not). If the root exists in any form it is refused. There is **no reuse and no continuation protocol**: an interrupted or finished run is never resumed, and a new run needs a new id. A half-copied registry therefore cannot survive into a later run.
- `scripts/ab-env.sh` (sourced for interactive checks of a finished run) accepts an existing root only if it is a real directory that resolves to itself, owned by the caller, mode 700, with a `.ab-root` marker naming the same id and real `tmp/ target/ cargo/ home/` subdirectories. A directory without that marker (for example one created by hand or by another tool) is refused in every mode.
- `/var/tmp` itself must be a plain directory that resolves to itself.
- `TMPDIR` is `/var/tmp/ab-<id>/tmp`, **at most 22 characters**, so `<id>` is at most 6 characters. Tests bind Unix sockets inside it; `tests/evidence.rs` `path_manifest_rejection` fails with `path must be shorter than SUN_LEN` at 23 characters (measured: 22 passes, 23 fails; the fixed suffix is 85 bytes). Examples that are too long: `my-build`, `long-name`. `TMPDIR` must contain no symlink (`src/execution.rs` requires the canonical form of a fixture path to equal the given path).

### Environment variables

The build environment is **constructed**, not inherited. Every cargo, rustc and test process runs in the sandbox below with `--clearenv` and exactly these variables:

| Variable | Value |
|---|---|
| `PATH` | pinned toolchain `bin/`, then `/usr/bin:/bin` |
| `RUSTC`, `RUSTDOC`, `RUSTFMT` | absolute paths of the pinned binaries |
| `CARGO_HOME` | `/var/tmp/ab-<id>/cargo`, seeded with `cp -a $AB_RUST_ROOT/cargo/registry` only (not `bin/`, not `env`) |
| `CARGO_TARGET_DIR` | `/var/tmp/ab-<id>/target` (never shared) |
| `TMPDIR`, `HOME` | `/var/tmp/ab-<id>/tmp`, `/var/tmp/ab-<id>/home` |
| `CARGO_NET_OFFLINE`, `CARGO_TERM_COLOR` | `true`, `never` (every cargo command also passes `--offline --locked`) |
| `DBUS_SESSION_BUS_ADDRESS` | `unix:path=/var/tmp/ab-<id>/no-bus`, a path that does not exist (defence in depth only; see below) |
| `USER`, `LOGNAME`, `LANG`, `TERM` | the caller's user name, `C.UTF-8`, `dumb` |

`XDG_RUNTIME_DIR` and everything else is absent. In addition, `ab-env.sh` **refuses to run** (naming the variable) when the caller's environment contains a variable that could change the compiler, linker, profile or inputs of a build: `RUST*` (including `RUSTC`, `RUSTDOC`, `RUSTFLAGS`, wrappers, `RUSTC_BOOTSTRAP`, `RUSTUP_*`), every `CARGO_*` (including `CARGO_BUILD_*`, `CARGO_PROFILE_*`, `CARGO_TARGET_<triple>_{LINKER,RUSTFLAGS}`, `CARGO_HOME`, `CARGO_TARGET_DIR`), `CC CXX CPP AR LD NM RANLIB STRIP OBJCOPY OBJDUMP`, `CFLAGS CXXFLAGS CPPFLAGS LDFLAGS ASFLAGS ARFLAGS`, the `cc` crate's `CC_* CXX_* AR_* CFLAGS_* CXXFLAGS_* LDFLAGS_* TARGET_* HOST_*`, `LD_*` (so `LD_PRELOAD`, `LD_LIBRARY_PATH`), `LIBRARY_PATH C_INCLUDE_PATH CPLUS_INCLUDE_PATH CPATH`, `SQLITE*`, `LIBSQLITE3*`, `PKG_CONFIG*`, `BASH_ENV`, `ENV`, and `GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CONFIG*`. Ambient variables outside that list (for example `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`, `HOME`, `TMPDIR`) are not trusted either: they are simply not passed into the sandbox. Two layers, so a variable cannot silently change the verification conditions: rejected up front where it signals intent, and unreachable regardless.

`ab-env.sh` assigns and exports nothing until every check has passed, and then only `AB_ID AB_BUILD_ROOT AB_REPO AB_RUST_ROOT AB_TOOLCHAIN_BIN AB_REGISTRY_SEED AB_ROOT_STATE`. A failed `source` leaves the caller's shell exactly as it was, and a successful one does not change `PATH`, `CARGO_*`, `TMPDIR` or `RUSTC` in it.

### Sandbox (`scripts/ab-sandbox.sh`)

One `bwrap` (bubblewrap, unprivileged user namespace) allow-list sandbox for every command: read-only `/usr`, `/etc`, the repository and the pinned toolchain tree; writable only the build root's `tmp/ target/ cargo/ home/`; fresh `/tmp`, `/run` and `/dev`; new network, PID, IPC and UTS namespaces; `--die-with-parent --new-session`; and, in front of `bwrap`, a descriptor-hygiene trampoline (below). Because the view is built from an allow-list, no `rustc`/`cargo` other than the pinned ones exists in it, no `.cargo` or `rust-toolchain*` exists in any ancestor of the repository, and the repository itself is read-only. The scripts refuse to run if `bwrap` is missing or cannot create the sandbox; there is no unisolated mode.

**The network namespace isolates IP networking only.** A user + network namespace does not hide filesystem Unix sockets: with the same uid, `/run/user/<uid>/bus`, the system bus, terminal-multiplexer and agent sockets and the sockets of any other local service stay reachable through the host's mount namespace. Unsetting `DBUS_SESSION_BUS_ADDRESS` or using `unshare --net` does not protect the desktop, and in this product unsetting has the opposite effect: with the variable unset, `src/dbus.rs` falls back to `/run/user/<uid>/bus`. The load-bearing mechanism is therefore the **mount** isolation: those paths do not exist inside the sandbox. `ab_boundary_probe` proves this before any cargo command and `ab-selftest.sh` proves that the probe can fail (it fails a network-namespace-only wrapper and a sandbox that re-exposes the real bus).

**The mount namespace hides paths, not inherited descriptors.** `bwrap` passes the launching process's open file descriptors straight through to the workload, so stdin and any socket, directory, file or pipe the launcher holds open would be reachable inside the sandbox regardless of the mount isolation. A directory descriptor is the sharpest case: an inherited `O_DIRECTORY` fd can be walked up to the host root and used to `fstatat` the real session bus and tmux sockets even though their paths do not exist inside. Closing the descriptors at the boundary is therefore the second load-bearing mechanism. `ab_sandbox_build` builds `AB_SANDBOX` as a small `bash` trampoline (`$_AB_FD_TRAMPOLINE`) in front of `bwrap`: it points stdin at `/dev/null` and closes every descriptor `>= 3`, preserving only stdout and stderr (the required output channel), then `exec`s `bwrap`. Because it `exec`s rather than forks, it adds no process and `--die-with-parent` still binds `bwrap` to the original caller. It fails closed — if it cannot read `/proc/$$/fd` or reopen stdin it refuses to launch. Doing this at the one place the sandbox is defined means every call site (`ab-verify.sh`, the probe, interactive use of `"${AB_SANDBOX[@]}"`) inherits the guarantee; an operator does not have to redirect stdin or close extra descriptors by hand. `AB_SANDBOX_RAW` is the same isolation *without* the trampoline and exists only as the probe's negative control. `ab_boundary_probe` deliberately inherits a synthetic socket, directory and pipe into the wrapper and checks that none survives inside, that no descriptor `>= 3` is inherited, and that stdin is not a socket, pipe or directory; `ab-selftest.sh` proves the check fails for `AB_SANDBOX_RAW`, that an inherited Unix socket is usable from inside without the trampoline and gone with it, that the trampoline fails closed, and that teardown is preserved.

Host requirements, not pinned: `bwrap` (tested: bubblewrap 0.12.0), `/usr/bin/bash` and `/dev/null` (descriptor-hygiene trampoline), unprivileged user namespaces, `python3` (probe only), a C compiler and linker, `git`, `strings`, `/bin/sleep`, `/bin/true`, and at least 10 GB free on `/var/tmp`.

The scripts refuse to proceed if any of these paths is under `/tmp`, or free space on `/var/tmp` is below 10 GB.

## Registry seed

The seed holds 46 `.crate` files (digest list: SHA-256 of the sorted `sha256sum *.crate` output, `0ae5507090fe8af4ee45054e41f7654d133f273514452c14c8856b6a221ed5db`) and an index cache entry for all 66 registry packages in `Cargo.lock`. It deliberately lacks 20 crates that exist in the lockfile only for other targets:

- wasm: `bumpalo-3.20.3`, `futures-core-0.3.34`, `futures-task-0.3.34`, `futures-util-0.3.34`, `js-sys-0.3.107`, `pin-project-lite-0.2.17`, `rsqlite-vfs-0.1.1`, `rustversion-1.0.23`, `slab-0.4.12`, `sqlite-wasm-rs-0.5.5`, `wasm-bindgen-0.2.130`, `wasm-bindgen-macro-0.2.130`, `wasm-bindgen-macro-support-0.2.130`, `wasm-bindgen-shared-0.2.130`
- Windows: `winapi-0.3.9`, `winapi-i686-pc-windows-gnu-0.4.0`, `winapi-x86_64-pc-windows-gnu-0.4.0`, `windows-link-0.2.1`, `windows-sys-0.61.2`
- UEFI: `r-efi-6.0.0`

`scripts/ab-verify.sh` proves they are not needed on Linux by running `cargo metadata --offline --locked --filter-platform x86_64-unknown-linux-gnu` before anything else; if that wants the network or a missing crate, the run stops. A cross-target build (wasm, Windows) is **not** supported by this seed.

## Use

```sh
AB_ID=<new-id> scripts/ab-verify.sh [evidence-file]   # full reproduction; exit 0 only if all required checks pass
AB_ID=<new-id> scripts/ab-selftest.sh                 # negative tests of the environment checks themselves; build-free
AB_ID=<id> . scripts/ab-env.sh                        # only validates; exports AB_* variables
AB_ID=<id> . scripts/ab-env.sh && . scripts/ab-sandbox.sh && ab_sandbox_build && "${AB_SANDBOX[@]}" -- cargo --version   # an interactive command in the sandbox
```

`AB_RUST_ROOT` must be set in the caller's environment for every one of these. `<new-id>` must not exist. The evidence file must be under `/var/tmp/ab-<id>/tmp` (default `verification.txt`); relative paths are taken from the caller's directory and resolved before the check. The worktree must be completely clean (tracked, untracked and ignored) before the run starts and is checked again afterwards, so a log is bound to the `head=` it records.

The required checks, all run inside the sandbox, are:

- the dependency proof (`cargo metadata`, see "Registry seed") and the boundary probe (bus unreachability **and** inherited-descriptor hygiene, with a synthetic socket, directory and pipe deliberately inherited and shown stripped), both before anything is compiled;
- `cargo fmt --check`;
- `cargo clippy --offline --locked --all-targets --all-features -- -D warnings`;
- `cargo test --offline --locked --features fault-injection`, and the same with `-- --test-threads=1`;
- `cargo build --offline --release --locked`;
- a check that cargo's recorded compiler identity is the pin;
- exact test accounting: **243 passed, 0 failed, 0 ignored** in both the parallel and the serial run;
- release checks: the three binaries (`orchestrator`, `supervisor`, `fake-agent`) were built and contain no `ASHER_TEST_` strings.

Release binaries are produced only by this build, under `/var/tmp/ab-<id>/target/release/`; the repository tracks no binary. The attended desktop smoke is not part of the verification: it sends real notifications.

## What the scripts write

The selftest reports 65 checks. Its refusals of an existing build root use a marker-less root that the selftest creates and removes itself, so it does not depend on anything that existed on a machine beforehand; it also checks that a missing `AB_RUST_ROOT` is refused, that a symlink under the code paths is refused, and that a one-byte source change is caught by the code digest.

The scripts write only `/var/tmp/ab-<id>/`. During the selftest they also create a few throwaway `/var/tmp/ab-*` entries of their own (roots and a symlink for the "existing root" refusals), which the selftest removes itself. Nothing under `AB_RUST_ROOT` and nothing in the repository is modified. To clean up after a run, remove that one build-root directory once you no longer need its log or binaries.
