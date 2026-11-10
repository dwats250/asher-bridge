#!/usr/bin/env bash
# Negative tests for the verification environment itself (D1 environment integrity,
# D2 build-root identity, D3 desktop-bus isolation, and the minor findings).
#
#   AB_ID=<new-id> scripts/ab-selftest.sh
#
# Safe by construction: it builds nothing, sends no notification and never connects to the owner's
# session bus. The real bus is only lstat-ed, and connected to only when lstat already reports it
# absent. The only listener is a decoy socket inside the selftest's own build root. It reserves
# /var/tmp/ab-<id> like ab-verify.sh does and writes nothing else, except for one transient symlink
# /var/tmp/ab-lnk (D2) that it creates and removes itself, and only if that path is free.
# Exit status is 0 only if every check passed.
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
ENV_SH="$here/ab-env.sh"; VERIFY_SH="$here/ab-verify.sh"
uid="$(id -u)"
SEL_ID="${AB_ID-}"

pass=0; nfail=0
ok()  { pass=$((pass+1)); printf 'PASS  %s\n' "$1"; }
bad() { nfail=$((nfail+1)); printf 'FAIL  %s\n' "$1"; }
chk() { if [ "$2" = 0 ]; then ok "$1"; else bad "$1${3:+ ($3)}"; fi; }

# the ab-* entries of /var/tmp (unrelated files there are not this script's concern)
vartmp_list() { ls -A /var/tmp | LC_ALL=C sort | grep -E '^ab-' || true; }
# a clean, minimal environment for sourcing ab-env.sh, plus whatever the caller adds
# (the toolchain location has no default, so it is the one thing carried over)
cleanenv() { env -i PATH=/usr/bin:/bin HOME=/nonexistent AB_RUST_ROOT="${AB_RUST_ROOT-}" AB_TOOLCHAIN_BIN="${AB_TOOLCHAIN_BIN-}" AB_REGISTRY_SEED="${AB_REGISTRY_SEED-}" "$@"; }

. "$ENV_SH" || { echo "ab-selftest: environment check failed (AB_ID='$SEL_ID'); nothing was run" >&2; exit 2; }
. "$here/ab-sandbox.sh"
[ "$AB_ROOT_STATE" = new ] || { echo "ab-selftest: build root $AB_BUILD_ROOT exists; use a new AB_ID" >&2; exit 2; }
vt_before="$(vartmp_list)"
mkdir -m 700 -- "$AB_BUILD_ROOT" || { echo "ab-selftest: cannot reserve $AB_BUILD_ROOT" >&2; exit 2; }
mkdir -m 700 "$AB_BUILD_ROOT"/{tmp,target,cargo,home}
{ echo "id=$AB_ID"; echo "uid=$uid"; echo "repo=$AB_REPO"; echo "head=$(git -C "$AB_REPO" rev-parse HEAD)"; echo "created=$(date -u +%FT%TZ) (selftest)"; } >"$AB_BUILD_ROOT/.ab-root"
R="$AB_BUILD_ROOT"; S="$R/tmp"
exec > >(tee "$S/selftest.txt") 2>&1

echo "Asher Bridge verification selftest. id=$AB_ID head=$(git -C "$AB_REPO" rev-parse HEAD) started=$(date -u +%FT%TZ)"
echo "scripts: $(cd "$here" && sha256sum ab-env.sh ab-sandbox.sh ab-verify.sh ab-selftest.sh | awk '{printf "%s=%s ", $2, substr($1,1,16)}')"

echo; echo "== D1: environment integrity"
cleanenv AB_ID="$AB_ID" bash -c '. "$1" >/dev/null 2>&1' _ "$ENV_SH"; chk "positive control: a clean environment passes ab-env.sh" $?
msg="$(env -i PATH=/usr/bin:/bin HOME=/nonexistent AB_ID="$AB_ID" bash -c '. "$1"' _ "$ENV_SH" 2>&1)"; rc=$?
[ $rc -ne 0 ] && grep -q 'AB_RUST_ROOT is required' <<<"$msg"; chk "AB_RUST_ROOT unset: refused, there is no built-in toolchain location (rc=$rc)" $?
hostile=(RUSTC RUSTDOC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER RUSTC_BOOTSTRAP RUSTFLAGS RUSTDOCFLAGS RUSTUP_TOOLCHAIN RUSTUP_HOME
  CARGO_HOME CARGO_TARGET_DIR CARGO_BUILD_RUSTC CARGO_BUILD_RUSTC_WRAPPER CARGO_BUILD_TARGET CARGO_BUILD_RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
  CARGO_PROFILE_RELEASE_OPT_LEVEL CARGO_PROFILE_DEV_DEBUG CARGO_INCREMENTAL CARGO_NET_OFFLINE
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS
  CC CXX CPP AR LD CFLAGS CXXFLAGS CPPFLAGS LDFLAGS CC_x86_64_unknown_linux_gnu CFLAGS_x86_64_unknown_linux_gnu TARGET_CC HOST_CC
  LD_PRELOAD LD_LIBRARY_PATH LIBRARY_PATH C_INCLUDE_PATH CPATH SQLITE3_LIB_DIR LIBSQLITE3_SYS_BUNDLED PKG_CONFIG_PATH BASH_ENV ENV
  GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_CONFIG_GLOBAL)
rej=0; for v in "${hostile[@]}"; do
  msg="$(cleanenv AB_ID="$AB_ID" "$v=/x" bash -c '. "$1"' _ "$ENV_SH" 2>&1 >/dev/null)"; rc=$?
  if [ $rc -ne 0 ] && grep -q "refusing to run with $v " <<<"$msg"; then rej=$((rej+1)); else bad "ab-env.sh did not reject $v (rc=$rc)"; fi
done
chk "ab-env.sh rejects each of ${#hostile[@]} compiler/linker/profile/steering variables by name" $((rej != ${#hostile[@]}))
cleanenv AB_ID="$AB_ID" NVM_CD_FLAGS=1 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus XDG_RUNTIME_DIR=/run/user/$uid TMPDIR=/tmp/x HOME=/home/x \
  bash -c '. "$1" >/dev/null 2>&1' _ "$ENV_SH"; chk "unrelated ambient variables (NVM_CD_FLAGS, DBUS_*, XDG_*, TMPDIR, HOME) are not rejected; the sandbox neutralizes them" $?

# regression: a logging RUSTC wrapper must never be invoked
cat >"$S/rustc-wrap.sh" <<EOF
#!/bin/sh
echo "wrapper invoked: \$*" >> "$S/wrap.log"
exec "$AB_TOOLCHAIN_BIN/rustc" "\$@"
EOF
chmod +x "$S/rustc-wrap.sh"; rm -f "$S/wrap.log"
cleanenv AB_ID="$AB_ID" RUSTC="$S/rustc-wrap.sh" bash -c '. "$1" >/dev/null 2>&1 && "$2" metadata --offline --locked --format-version 1 >/dev/null 2>&1' _ "$ENV_SH" "$AB_TOOLCHAIN_BIN/cargo"
rc=$?; chk "RUSTC=<logging wrapper>: ab-env.sh fails (rc=$rc)" $((rc == 0))
[ ! -e "$S/wrap.log" ]; chk "the wrapper was never invoked (its log does not exist)" $?

# M3: a failed source changes nothing in the caller; a successful one adds only AB_* variables
r1="$(RUSTC=/x CC=/y bash -c 'b=$(env|LC_ALL=C sort|md5sum); AB_ID='"$AB_ID"' . "$1" >/dev/null 2>&1; rc=$?; a=$(env|LC_ALL=C sort|md5sum); [ $rc -ne 0 ] && [ "$a" = "$b" ] && echo same' _ "$ENV_SH")"
[ "$r1" = same ]; chk "M3: a failed source leaves the caller's environment byte-identical" $?
r2="$(cleanenv bash -c 'b=$(env|grep -v ^AB_|LC_ALL=C sort|md5sum); AB_ID=nwx2 . "$1" >/dev/null 2>&1; rc=$?; a=$(env|grep -v ^AB_|LC_ALL=C sort|md5sum); p="$PATH"; [ $rc -eq 0 ] && [ "$a" = "$b" ] && [ -z "${CARGO_HOME-}${TMPDIR-}${RUSTC-}" ] && [ "$p" = /usr/bin:/bin ] && echo same' _ "$ENV_SH")"
[ "$r2" = same ]; chk "a successful source exports only AB_* (no PATH, CARGO_*, TMPDIR, RUSTC change)" $?

# neutralization inside the sandbox: hostile parent environment, nothing may arrive
ab_sandbox_build || exit 2
hostile_env=(RUSTC=/evil/rustc RUSTDOC=/evil/rustdoc RUSTFLAGS=-Cevil CARGO_BUILD_RUSTC=/evil CARGO_PROFILE_RELEASE_OPT_LEVEL=0 CARGO_HOME=/evil/cargo
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/evil/ld CC=/evil/cc CFLAGS=-evil AR=/evil/ar LD_LIBRARY_PATH=/evil SQLITE3_LIB_DIR=/evil
  GIT_DIR=/evil DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus XDG_RUNTIME_DIR=/run/user/$uid HOME=/evil/home TMPDIR=/evil/tmp PATH=/evil/bin:/usr/bin)
envout="$(env "${hostile_env[@]}" "${AB_SANDBOX[@]}" -- /usr/bin/env 2>&1)"
leaks=0
for kv in "${hostile_env[@]}"; do
  if grep -qxF -- "$kv" <<<"$envout"; then leaks=$((leaks+1)); echo "    leaked into sandbox: $kv"; fi
done
chk "no hostile ambient variable (${#hostile_env[@]} tried) reaches the sandbox with its hostile value" $leaks
unexpected="$(grep -vE '^(PATH|HOME|USER|LOGNAME|LANG|TERM|CARGO_HOME|CARGO_TARGET_DIR|TMPDIR|CARGO_NET_OFFLINE|CARGO_TERM_COLOR|RUSTC|RUSTDOC|RUSTFMT|DBUS_SESSION_BUS_ADDRESS|PWD|SHLVL|_|OLDPWD)=' <<<"$envout")"
chk "the sandbox environment holds only the allow-listed names" $([ -z "$unexpected" ]; echo $?) "$unexpected"
grep -qx "RUSTC=$AB_TOOLCHAIN_BIN/rustc" <<<"$envout" && grep -qx "DBUS_SESSION_BUS_ADDRESS=unix:path=$R/no-bus" <<<"$envout" && ! grep -q '^XDG_RUNTIME_DIR=' <<<"$envout"
chk "inside: RUSTC is the pinned compiler, the bus address is the dead path, XDG_RUNTIME_DIR is unset" $?
v="$(env "${hostile_env[@]}" "${AB_SANDBOX[@]}" -- sh -c 'command -v rustc; rustc --version; command -v cargo; cargo --version; ls /usr/bin /bin | grep -ciE "^(rustc|cargo)$"')"
[ "$v" = "$(printf '%s\n' "$AB_TOOLCHAIN_BIN/rustc" 'rustc 1.99.1 (58cf8f9c3 2026-10-13)' "$AB_TOOLCHAIN_BIN/cargo" 'cargo 1.99.1 (5f94df478 2026-08-27)' 0)" ]
chk "inside, under a hostile parent: rustc and cargo resolve to the pinned binaries and the host has no other rustc/cargo in /usr/bin" $?
anc=(); a="$AB_REPO"; while [ "$a" != / ]; do anc+=("$a"); a="$(dirname "$a")"; done
out="$("${AB_SANDBOX[@]}" -- sh -c 'for a; do for f in .cargo rust-toolchain rust-toolchain.toml; do [ -e "$a/$f" ] && echo "$a/$f"; done; done; echo end' sh "${anc[@]}" /)"
chk "inside: no .cargo or rust-toolchain exists in the repository or any of its ancestors" $([ "$out" = end ]; echo $?) "$out"

# steering-file discovery on a scratch tree
mkdir -p "$S/steer/ok" "$S/steer/bad/.cargo" "$S/steer/bad2"; : >"$S/steer/bad/.cargo/config.toml"; : >"$S/steer/bad2/rust-toolchain.toml"; : >"$S/steer/bad2/build.rs"
[ -z "$(ab_find_steering "$S/steer/ok")" ]; chk "ab_find_steering: a clean tree reports nothing" $?
n="$(ab_find_steering "$S/steer/bad")$(ab_find_steering "$S/steer/bad2")"; [ -n "$(ab_find_steering "$S/steer/bad")" ] && [ -n "$(ab_find_steering "$S/steer/bad2")" ]
chk "ab_find_steering: .cargo and rust-toolchain.toml found" $?
: >"$R/cargo/config.toml"; cleanenv AB_ID="$AB_ID" bash -c '. "$1" >/dev/null 2>&1' _ "$ENV_SH"; rc=$?; rm -f "$R/cargo/config.toml"
chk "a config.toml in CARGO_HOME is rejected" $((rc == 0))
# M6: the whole-tree digest catches a toolchain that only matches the spot-checked binaries
mkdir -p "$S/farm/bin"; for f in "$AB_TOOLCHAIN_BIN"/*; do ln -s "$f" "$S/farm/bin/$(basename "$f")"; done
msg="$(cleanenv AB_ID="$AB_ID" AB_TOOLCHAIN_BIN="$S/farm/bin" bash -c '. "$1"' _ "$ENV_SH" 2>&1 >/dev/null)"; rc=$?
[ $rc -ne 0 ] && grep -q 'toolchain tree digest' <<<"$msg"; chk "M6: a look-alike toolchain with the pinned binaries but a different tree is rejected by the tree digest" $?
rm -rf "$S/farm"

echo; echo "== D2: build-root identity"
msg="$(cleanenv bash -c '. "$1"' _ "$ENV_SH" 2>&1)"; rc=$?
[ $rc -ne 0 ] && grep -q 'AB_ID is required' <<<"$msg"; chk "AB_ID unset: refused, no default (rc=$rc)" $?
env -u AB_ID "$VERIFY_SH" >/dev/null 2>&1; rc=$?; chk "AB_ID unset: ab-verify.sh exits 2 and runs nothing" $((rc != 2))
for id in '' A '../x' 'a b' -x toolong7 'a/b' 'ab.c'; do
  AB_ID="$id" "$VERIFY_SH" >/dev/null 2>&1; rc=$?; chk "invalid AB_ID '$id': ab-verify.sh refuses (rc=$rc)" $((rc != 2))
done
# A root that already exists and was not created by a verification run (no .ab-root marker) stands
# in for someone else's directory. It is created here, only if the name is free, and removed again.
if [ ! -e /var/tmp/ab-nomk ] && [ ! -L /var/tmp/ab-nomk ]; then
  mkdir -m 700 /var/tmp/ab-nomk && echo keep >/var/tmp/ab-nomk/sentinel
  nomk_before="$(cd /var/tmp/ab-nomk && find . -printf '%p %s %T@\n' | LC_ALL=C sort | sha256sum)"
  AB_ID=nomk "$VERIFY_SH" >"$S/nomk.out" 2>&1; rc=$?
  chk "an existing root without a .ab-root marker is refused (rc=$rc)" $((rc != 2)) "$(head -2 "$S/nomk.out")"
  [ "$(cd /var/tmp/ab-nomk && find . -printf '%p %s %T@\n' | LC_ALL=C sort | sha256sum)" = "$nomk_before" ]; chk "the refused run left the foreign root unchanged (listing digest)" $?
  rm -f /var/tmp/ab-nomk/sentinel; rmdir /var/tmp/ab-nomk
else bad "/var/tmp/ab-nomk already exists; foreign-root test not run"; fi
before="$(find "$R" -maxdepth 1 -printf '%p %s\n' | LC_ALL=C sort | sha256sum)"; marker_before="$(sha256sum "$R/.ab-root")"
AB_ID="$AB_ID" "$VERIFY_SH" >"$S/own.out" 2>&1; rc=$?
chk "re-running ab-verify.sh on an existing own root is refused: no reuse, no continuation (rc=$rc)" $((rc != 2)) "$(head -1 "$S/own.out")"
[ "$(find "$R" -maxdepth 1 -printf '%p %s\n' | LC_ALL=C sort | sha256sum)" = "$before" ] && [ "$(sha256sum "$R/.ab-root")" = "$marker_before" ]; chk "the refused rerun changed neither the root's entries nor its marker" $?
if [ ! -e /var/tmp/ab-lnk ] && [ ! -L /var/tmp/ab-lnk ]; then
  mkdir "$S/linktarget"; ln -s "$S/linktarget" /var/tmp/ab-lnk
  AB_ID=lnk "$VERIFY_SH" >"$S/lnk.out" 2>&1; rc=$?
  chk "a symlinked destination (existing target) is refused (rc=$rc)" $((rc != 2)) "$(head -1 "$S/lnk.out")"
  [ -z "$(ls -A "$S/linktarget")" ]; chk "nothing was written through the symlink" $?
  rm -f /var/tmp/ab-lnk; ln -s "$S/no-such-target" /var/tmp/ab-lnk
  AB_ID=lnk "$VERIFY_SH" >/dev/null 2>&1; rc=$?; chk "a dangling symlink destination is refused (rc=$rc)" $((rc != 2))
  [ ! -e "$S/no-such-target" ]; chk "nothing was created at the dangling target" $?
  [ -L /var/tmp/ab-lnk ] && rm -f /var/tmp/ab-lnk
else bad "/var/tmp/ab-lnk already exists; symlink test not run"; fi
for ev in /var/../tmp/x //tmp/x ../x /etc/x; do
  AB_ID=ev1 "$VERIFY_SH" "$ev" >"$S/ev.out" 2>&1; rc=$?
  chk "M2: evidence path '$ev' is refused (rc=$rc)" $((rc != 2)) "$(head -1 "$S/ev.out")"
done
[ ! -e /var/tmp/ab-ev1 ]; chk "M2: nothing was reserved for the refused evidence paths" $?

echo; echo "== D3: desktop-bus isolation (non-notifying)"
if ab_boundary_probe "${AB_SANDBOX[@]}" --; then ok "D3: the real sandbox passes the boundary probe"; else bad "D3: the real sandbox FAILED the boundary probe"; fi
echo "-- negative control 1: the OLD mechanism (user + network namespace only) must FAIL the same probe"
if ab_boundary_probe unshare --user --map-user="$uid" --net --; then bad "D3: the probe passed a network-namespace-only wrapper, so it cannot detect that weakness"; else ok "D3: the probe FAILS a network-namespace-only wrapper (the weakness is detected)"; fi
echo "-- negative control 2: a sandbox that re-exposes the real bus socket must FAIL the probe (stat only, no connect)"
ab_sandbox_build --ro-bind-try "/run/user/$uid/bus" "/run/user/$uid/bus"; leaky=("${AB_SANDBOX[@]}")
if ab_boundary_probe "${leaky[@]}" --; then bad "D3: the probe passed a sandbox that exposes the real bus"; else ok "D3: the probe FAILS a sandbox that exposes the real bus"; fi
ab_sandbox_build
echo "-- positive re-check after the controls"
if ab_boundary_probe "${AB_SANDBOX[@]}" --; then ok "D3: the real sandbox still passes after the controls"; else bad "D3: the real sandbox failed on re-check"; fi

echo; echo "== D4: inherited file-descriptor hygiene"
# The probe now also inherits a synthetic socket, directory and pipe into the wrapper and checks they
# are stripped. The supported sandbox must pass; the raw sandbox (the same mount/namespace isolation
# without the fd trampoline) must fail on exactly those checks, proving the trampoline is load-bearing
# and the fd check is not vacuous.
ab_sandbox_build
if ab_boundary_probe "${AB_SANDBOX[@]}" -- >"$S/d4-supported.txt" 2>&1; then ok "D4: the supported sandbox passes the boundary probe (inherited-fd checks included)"; else bad "D4: the supported sandbox FAILED the boundary probe"; fi
grep -qF 'ok   no inherited file descriptor >= 3 (none)' "$S/d4-supported.txt"; chk "D4: inside the supported sandbox no fd >= 3 is inherited" $?
grep -qF 'ok   fd 0 (stdin) is not a socket, pipe or directory: /dev/null' "$S/d4-supported.txt"; chk "D4: inside the supported sandbox stdin is /dev/null" $?
grep -qF 'ok   no inherited socket or directory descriptor on any fd (none)' "$S/d4-supported.txt"; chk "D4: inside the supported sandbox no socket or directory fd is inherited" $?
echo "-- negative control: the raw sandbox (no fd trampoline) must FAIL the inherited-fd checks"
if ab_boundary_probe "${AB_SANDBOX_RAW[@]}" -- >"$S/d4-raw.txt" 2>&1; then bad "D4: the probe PASSED a sandbox without fd hygiene; it cannot detect inherited fds"; else ok "D4: the probe FAILS the raw sandbox without the fd trampoline"; fi
grep -qF 'FAIL no inherited file descriptor >= 3' "$S/d4-raw.txt"; chk "D4: the raw sandbox leaks the synthetic socket/dir/pipe and the probe reports it" $?

echo "-- demonstration: a deliberately inherited Unix socket is usable without the trampoline and gone with it"
cat >"$S/d4_sendprobe.py" <<'PY'
import os, socket, sys, time
sp, lp = sys.argv[1], sys.argv[2]; argv = sys.argv[3:]
srv = socket.socket(socket.AF_UNIX)
try: os.unlink(sp)
except FileNotFoundError: pass
srv.bind(sp); srv.listen(1)
pid = os.fork()
if pid == 0:
    srv.settimeout(6); log = open(lp, "w", buffering=1)
    try:
        c, _ = srv.accept(); c.settimeout(5); data = c.recv(64)
        log.write(("RECV %r\n" % data) if data else "EOF\n")
    except socket.timeout:
        log.write("NOCONN\n")
    os._exit(0)
srv.close(); time.sleep(0.2)
cli = socket.socket(socket.AF_UNIX); cli.connect(sp)
os.dup2(cli.fileno(), 3); os.set_inheritable(3, True)
os.execv(argv[0], argv)
PY
cat >"$S/d4_trysend.py" <<'PY'
import os, socket
try:
    s = socket.socket(fileno=os.dup(3)); s.send(b"LEAKED"); print("INSIDE: fd 3 usable")
except OSError as e:
    print("INSIDE: fd 3 unusable: %s" % e.__class__.__name__)
PY
/usr/bin/python3 -I "$S/d4_sendprobe.py" "$S/d4raw.sock" "$S/d4raw.log" "${AB_SANDBOX_RAW[@]}" -- /usr/bin/python3 -I "$S/d4_trysend.py" >"$S/d4raw.out" 2>&1
grep -q "RECV b'LEAKED'" "$S/d4raw.log"; chk "D4: WITHOUT the trampoline the inherited Unix socket is usable from inside (host-only resource reached)" $? "$(cat "$S/d4raw.log" "$S/d4raw.out" 2>/dev/null | tr '\n' ' ')"
/usr/bin/python3 -I "$S/d4_sendprobe.py" "$S/d4sup.sock" "$S/d4sup.log" "${AB_SANDBOX[@]}" -- /usr/bin/python3 -I "$S/d4_trysend.py" >"$S/d4sup.out" 2>&1
{ ! grep -q RECV "$S/d4sup.log"; } && grep -q 'unusable' "$S/d4sup.out"; chk "D4: WITH the trampoline the same inherited Unix socket is gone and unusable from inside" $? "$(cat "$S/d4sup.log" "$S/d4sup.out" 2>/dev/null | tr '\n' ' ')"
rm -f "$S/d4raw.sock" "$S/d4sup.sock"

echo "-- fail-closed: the trampoline refuses to launch when stdin cannot be made /dev/null"
fcout="$(/usr/bin/bwrap --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 --proc /proc --tmpfs /dev -- /usr/bin/bash --noprofile --norc -c "$_AB_FD_TRAMPOLINE" ab-sandbox /usr/bin/printf RAN 2>&1)"; fcrc=$?
{ [ "$fcrc" -ne 0 ] && ! grep -q RAN <<<"$fcout"; }; chk "D4: the trampoline fails closed (no exec) when /dev/null is unavailable (rc=$fcrc)" $? "$fcout"

echo "-- teardown: the trampoline execs (adds no process), so --die-with-parent still tears the sandbox down"
"${AB_SANDBOX[@]}" -- /bin/sleep 191 >/dev/null 2>&1 & bgpid=$!
for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f '[s]leep 191' >/dev/null && break; sleep 0.2; done
kill -KILL "$bgpid" 2>/dev/null; wait "$bgpid" 2>/dev/null
for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f '[s]leep 191' >/dev/null || break; sleep 0.2; done
{ ! pgrep -f '[s]leep 191' >/dev/null; }; chk "D4: after the launcher is killed no sandboxed process survives (--die-with-parent preserved)" $?

echo; echo "== M1: untracked and ignored files cannot hide from the clean-tree gate"
g="$S/gitscratch"; mkdir "$g"; git -C "$g" init -q . && printf 'ignored.txt\n' >"$g/.gitignore" && : >"$g/a.rs" \
  && git -C "$g" add -A && git -c user.name=t -c user.email=t@t -c commit.gpgsign=false -C "$g" commit -q -m c
ab_require_clean_tree "$g" 2>/dev/null; chk "M1: clean scratch repository passes" $?
: >"$g/build.rs"; ab_require_clean_tree "$g" 2>/dev/null; chk "M1: an untracked build.rs is caught" $((! $?)); rm -f "$g/build.rs"
mkdir -p "$g/.cargo"; : >"$g/.cargo/config.toml"; ab_require_clean_tree "$g" 2>/dev/null; chk "M1: an untracked .cargo/config.toml is caught" $((! $?)); rm -rf "$g/.cargo"
: >"$g/ignored.txt"; ab_require_clean_tree "$g" 2>/dev/null; chk "M1: an ignored file is caught" $((! $?)); rm -f "$g/ignored.txt"
echo x >"$g/a.rs"; ab_require_clean_tree "$g" 2>/dev/null; chk "M1: a tracked modification is caught" $((! $?))

echo; echo "== code pin: only regular files are hashed, anything else under the code paths is refused"
pr="$S/pinrepo"; mkdir -p "$pr/scripts" && cp -a "$AB_REPO"/src "$AB_REPO"/tests "$AB_REPO"/migrations "$AB_REPO"/Cargo.toml "$AB_REPO"/Cargo.lock "$pr/" && cp "$ENV_SH" "$pr/scripts/ab-env.sh" \
  && git -C "$pr" init -q . && git -C "$pr" add -A && git -c user.name=t -c user.email=t@t -c commit.gpgsign=false -C "$pr" commit -q -m c
cleanenv AB_ID="$AB_ID" bash -c '. "$1" >/dev/null 2>&1' _ "$pr/scripts/ab-env.sh"; chk "pin: a scratch copy of the code paths passes ab-env.sh (positive control)" $?
ln -s ../Cargo.toml "$pr/tests/zz_link.rs"
msg="$(cleanenv AB_ID="$AB_ID" bash -c '. "$1"' _ "$pr/scripts/ab-env.sh" 2>&1 >/dev/null)"; rc=$?
[ $rc -ne 0 ] && grep -q 'non-regular entries in code paths' <<<"$msg"; chk "pin: a symlink under the code paths is refused by name (rc=$rc)" $?
rm -f "$pr/tests/zz_link.rs"
echo x >>"$pr/src/lib.rs"
msg="$(cleanenv AB_ID="$AB_ID" bash -c '. "$1"' _ "$pr/scripts/ab-env.sh" 2>&1 >/dev/null)"; rc=$?
[ $rc -ne 0 ] && grep -q 'code digest' <<<"$msg"; chk "pin: a one-byte change to a source file is refused by the code digest (rc=$rc)" $?

echo; echo "== environment after the selftest"
[ -z "$(ls -A "$R/decoy" 2>/dev/null | grep -v accept.log)" ]; chk "no decoy socket left behind" $?
leftover=""; for d in /proc/[0-9]*; do pid="${d#/proc/}"; [ "$pid" = "$$" ] && continue
  c="$(readlink "$d/cwd" 2>/dev/null)"; e="$(readlink "$d/exe" 2>/dev/null)"; case "$c$e" in *"$R"*) leftover="$leftover $pid" ;; esac; done
[ -z "$leftover" ]; chk "no leftover process with cwd or exe in the selftest root" $? "$leftover"
vt_after="$(vartmp_list)"
added="$(LC_ALL=C comm -13 <(printf '%s\n' "$vt_before") <(printf '%s\n' "$vt_after") | tr '\n' ' ')"; removed="$(LC_ALL=C comm -23 <(printf '%s\n' "$vt_before") <(printf '%s\n' "$vt_after") | tr '\n' ' ')"
[ "$added" = "ab-$AB_ID " ] && [ -z "$removed" ]; chk "the only ab-* entry added to /var/tmp is this run's root, and none was removed" $? "added='$added' removed='$removed'"

echo; echo "SELFTEST: passed=$pass failed=$nfail finished=$(date -u +%FT%TZ)"
if [ "$nfail" -eq 0 ]; then echo "RESULT: PASS"; exit 0; else echo "RESULT: FAIL"; exit 1; fi
