# Asher Bridge build-environment checks. Source it; do not execute it.
#
#   AB_ID=<id> . scripts/ab-env.sh
#
# Validates, and on success exports only AB_* variables describing, one verification run's
# environment. It sets no PATH, CARGO_*, TMPDIR, RUSTC or DBUS variable in the caller's shell:
# the build environment is constructed from scratch inside the sandbox by scripts/ab-sandbox.sh.
# It installs nothing, uses no network and writes nothing.
#
# It returns non-zero (the caller must stop) and leaves the caller's environment exactly as it
# found it if anything differs from the pin recorded in docs/TOOLCHAIN.md, if AB_ID is missing or
# invalid, if the build root exists and is not an intact root created by a verification run for
# this same id, or if the caller's environment carries a variable that could change what is built.
#
# Functions defined for scripts/ab-verify.sh and scripts/ab-selftest.sh:
#   ab_find_steering DIR     print build-steering files below DIR (.cargo, rust-toolchain*, build.rs)
#   ab_require_clean_tree D  succeed only if D has no tracked, untracked or ignored changes

ab_find_steering() {
  /usr/bin/find "$1" -path "$1/.git" -prune -o \
    \( -name .cargo -o -name rust-toolchain -o -name rust-toolchain.toml -o -path "$1/build.rs" \) -print 2>/dev/null
}

ab_require_clean_tree() {
  local out
  out="$(/usr/bin/git -C "$1" status --porcelain --untracked-files=all --ignored 2>&1)" || { echo "ab-env: git status failed in $1: $out" >&2; return 1; }
  [ -z "$out" ] || { echo "ab-env: worktree is not clean (tracked, untracked or ignored entries):" >&2; printf '%s\n' "$out" | head -20 >&2; return 1; }
}

_ab_env_main() {
  local -x PATH=/usr/bin:/bin
  local bad=0 repo id root uid v want got tree n ok_id=0 state=new
  _ab_fail() { echo "ab-env: $*" >&2; bad=1; }

  repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)" || { _ab_fail "cannot locate repository"; return 1; }
  uid="$(id -u)"

  # --- D2: explicit, valid, fresh-or-own build-root identity --------------------------------
  id="${AB_ID-}"
  if [ -z "$id" ]; then
    _ab_fail "AB_ID is required; there is no default. Choose a new id of at most 6 characters [a-z0-9-]"
  else
    case "$id" in
      *[!a-z0-9-]*|-*) _ab_fail "AB_ID must be lowercase [a-z0-9-] and not start with '-': '$id'" ;;
      *) if [ "${#id}" -gt 6 ]; then
           # TMPDIR=/var/tmp/ab-<id>/tmp must stay at 22 characters or fewer (docs/TOOLCHAIN.md).
           _ab_fail "AB_ID '$id' is ${#id} characters; at most 6 (TMPDIR would exceed 22 characters)"
         else ok_id=1; fi ;;
    esac
  fi
  root="/var/tmp/ab-$id"
  if [ "$(realpath -e /var/tmp 2>/dev/null)" != /var/tmp ] || [ -L /var/tmp ]; then
    _ab_fail "/var/tmp is not a plain directory (it is a symlink or resolves elsewhere)"
  fi
  if [ "$ok_id" = 1 ] && { [ -e "$root" ] || [ -L "$root" ]; }; then
    state=owned
    if [ -L "$root" ]; then _ab_fail "build root $root is a symlink; refused"
    elif [ ! -d "$root" ]; then _ab_fail "build root $root exists and is not a directory; refused"
    elif [ "$(realpath -e "$root")" != "$root" ]; then _ab_fail "build root $root does not resolve to itself; refused"
    elif [ "$(stat -c %u "$root")" != "$uid" ] || [ "$(stat -c %a "$root")" != 700 ]; then
      _ab_fail "build root $root is not owned by uid $uid with mode 700; refused"
    elif [ -L "$root/.ab-root" ] || [ ! -f "$root/.ab-root" ] || ! grep -qx "id=$id" "$root/.ab-root"; then
      _ab_fail "build root $root exists but is not a root created by a verification run for id '$id' (no matching .ab-root marker): it belongs to someone else or to history; choose a new id"
    else
      for v in tmp target cargo home; do
        [ -d "$root/$v" ] && [ ! -L "$root/$v" ] || _ab_fail "build root $root/$v is missing or a symlink; refused"
      done
    fi
  fi

  # --- D1: refuse anything in the caller's environment that could steer the build ------------
  for v in $(compgen -e); do
    case "$v" in
      RUST*|CARGO|CARGO_*|CC|CXX|CPP|AR|LD|NM|RANLIB|STRIP|OBJCOPY|OBJDUMP|CFLAGS|CXXFLAGS|CPPFLAGS|LDFLAGS|ASFLAGS|ARFLAGS|CC_*|CXX_*|AR_*|CFLAGS_*|CXXFLAGS_*|LDFLAGS_*|TARGET_*|HOST_*|LD_*|\
      LIBRARY_PATH|C_INCLUDE_PATH|CPLUS_INCLUDE_PATH|CPATH|SQLITE*|LIBSQLITE3*|PKG_CONFIG*|BASH_ENV|ENV|\
      GIT_DIR|GIT_WORK_TREE|GIT_INDEX_FILE|GIT_OBJECT_DIRECTORY|GIT_ALTERNATE_OBJECT_DIRECTORIES|GIT_COMMON_DIR|GIT_NAMESPACE|GIT_CONFIG*)
        _ab_fail "refusing to run with $v set in the environment: it can change the compiler, linker, profile or inputs of the build; unset it" ;;
    esac
  done
  # Files that steer cargo or rustup: inside the repository, or in the build root's CARGO_HOME.
  while IFS= read -r v; do [ -n "$v" ] && _ab_fail "build-steering path in the repository: $v"; done < <(ab_find_steering "$repo")
  for v in "$root/cargo/config" "$root/cargo/config.toml"; do [ -e "$v" ] && _ab_fail "CARGO_HOME config file present: $v"; done

  # --- pin (docs/TOOLCHAIN.md) -------------------------------------------------------------
  local rust_root bin seed
  # There is no built-in toolchain location: the caller names it.
  rust_root="${AB_RUST_ROOT-}"
  if [ -z "$rust_root" ] && { [ -z "${AB_TOOLCHAIN_BIN-}" ] || [ -z "${AB_REGISTRY_SEED-}" ]; }; then
    _ab_fail "AB_RUST_ROOT is required; there is no default. Set it to the directory holding rustup/toolchains/stable-x86_64-unknown-linux-gnu/ and cargo/registry/ (docs/TOOLCHAIN.md)"
  fi
  bin="${AB_TOOLCHAIN_BIN:-$rust_root/rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin}"
  seed="${AB_REGISTRY_SEED:-$rust_root/cargo/registry}"
  tree="$(dirname "$bin")"
  local pin_lock=f7751a1bd52ab4ce528700c558f878476dcd6e5cfa31707dc2cc2cad218f33e0
  local pin_code=f6fbea3901f9a899e39eb6b1cb8c3880b715d8b9142e49e769fffc55ab736010
  local pin_tree=906ed6e9af1f2c119863d71586ef07dc3667d5d08034566fd9c7e1b3fc467c6f

  if [ ! -d "$bin" ]; then _ab_fail "toolchain bin not found: $bin"; else
    _chk() { # name expected command...
      local n="$1" w="$2"; shift 2
      local g; g="$("$@" 2>&1 | head -1)"
      [ "$g" = "$w" ] || _ab_fail "$n is '$g', expected '$w'"
    }
    _chk rustc    'rustc 1.99.1 (58cf8f9c3 2026-10-13)'           "$bin/rustc" --version
    _chk cargo    'cargo 1.99.1 (5f94df478 2026-08-27)'           "$bin/cargo" --version
    _chk rustfmt  'rustfmt 1.10.0-stable (58cf8f9c35 2026-10-13)' "$bin/rustfmt" --version
    _chk clippy   'clippy 0.1.99 (58cf8f9c35 2026-10-13)'         "$bin/cargo-clippy" --version
    _chk rustdoc  'rustdoc 1.99.1 (58cf8f9c3 2026-10-13)'         "$bin/rustdoc" --version
    unset -f _chk
    local f
    while read -r want f; do
      got="$(sha256sum "$bin/$f" 2>/dev/null | cut -c1-64)"
      [ "$got" = "$want" ] || _ab_fail "$f binary digest '$got' differs from the pin $want"
    done <<'EOF'
b53b780f77f952062f8ca8a3b70aef2390b1e45ad91939be94afdc9543026853 rustc
a670e02cfa39ac2e53f77a2d911ca1fc9e2c479b0603ec956625be1daf24e3af cargo
4255f7671a17fba900ffd84a20f1dd0c986d52a2404496d3b7518f5044de5663 cargo-clippy
89b6cc8d0d988b00a3f65bc90c76fedbaedb6bcb442b34563aefe3621f26544b clippy-driver
68cb47a2a38daa8ff2ec6d623a2374b0a165d77ec74f1f654216665018c04598 rustfmt
832e53ca262db3d3fcaca659dc6e71f0f5a924a48adc580ee4278acac75f3e80 rustdoc
6b42f43e4523abba428eb42b3a106727843428b096ef7ed8a9497bddcc50448a cargo-fmt
EOF
    # The whole toolchain tree: every regular file's digest and every symlink, sorted (166 files, 0 links).
    got="$(cd "$tree" && { find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum; find . -type l -printf '%p -> %l\n' | LC_ALL=C sort; } | sha256sum | cut -c1-64)"
    [ "$got" = "$pin_tree" ] || _ab_fail "toolchain tree digest '$got' differs from the pin $pin_tree"
  fi

  [ "$(sha256sum "$repo/Cargo.lock" | cut -c1-64)" = "$pin_lock" ] || _ab_fail "Cargo.lock digest differs from the pin"
  # The code pin is a digest of file contents, so it does not depend on repository history:
  # the sorted per-file SHA-256 list of the code paths, hashed once more.
  # Only regular files are hashed, so anything else under the code paths (a symlink cargo could
  # follow to an unhashed file, a device, a socket) is refused rather than left out of the digest.
  n="$(cd "$repo" && find src tests migrations Cargo.toml Cargo.lock ! -type f ! -type d 2>/dev/null | head -5 | tr '\n' ';')"
  [ -z "$n" ] || _ab_fail "non-regular entries in code paths (not covered by the digest): $n"
  got="$(cd "$repo" && find src tests migrations Cargo.toml Cargo.lock -type f -print0 2>/dev/null | LC_ALL=C sort -z | xargs -0 sha256sum 2>/dev/null | sha256sum | cut -c1-64)"
  [ "$got" = "$pin_code" ] || _ab_fail "code digest '$got' differs from the pin $pin_code (src, tests, migrations, Cargo.toml, Cargo.lock)"
  n="$(git -C "$repo" status --porcelain --untracked-files=all --ignored -- src tests Cargo.toml Cargo.lock migrations 2>&1)" || _ab_fail "git status failed in $repo (a Git checkout is required): $n"
  [ -z "$n" ] || _ab_fail "uncommitted, untracked or ignored entries in code paths: $(printf '%s' "$n" | head -5 | tr '\n' ';')"

  n="$(df -Pk /var/tmp | awk 'NR==2{print $4}')"
  [ "${n:-0}" -ge 10485760 ] || _ab_fail "free space on /var/tmp below 10 GB"

  unset -f _ab_fail
  if [ "$bad" -ne 0 ]; then echo "ab-env: FAILED (see messages above); nothing was exported; do not build" >&2; return 1; fi
  AB_ID="$id"; AB_BUILD_ROOT="$root"; AB_REPO="$repo"; AB_RUST_ROOT="$rust_root"
  AB_TOOLCHAIN_BIN="$bin"; AB_REGISTRY_SEED="$seed"; AB_ROOT_STATE="$state"
  export AB_ID AB_BUILD_ROOT AB_REPO AB_RUST_ROOT AB_TOOLCHAIN_BIN AB_REGISTRY_SEED AB_ROOT_STATE
  echo "ab-env: ok id=$id root=$root root_state=$state code=$pin_code head=$(git -C "$repo" rev-parse HEAD)"
}

_ab_env_main
_ab_env_rc=$?
unset -f _ab_env_main
return "$_ab_env_rc" 2>/dev/null || exit "$_ab_env_rc"
