#!/usr/bin/env bash
# Run the Asher Bridge verification offline in a fresh, isolated build root.
#
#   AB_ID=<new-id> scripts/ab-verify.sh [evidence-file]
#
# AB_ID is required and must name a build root that does not exist (/var/tmp/ab-<id>). There is no
# default and no reuse or continuation: an existing, ambiguous or symlinked destination is refused,
# and a rerun needs a new id. The root is reserved atomically (mkdir without -p) before anything is
# written. Every cargo and test process runs in the sandbox of scripts/ab-sandbox.sh, after a
# non-notifying probe has shown that the owner's session bus is unreachable from it.
#
# Optional environment:
#   AB_REPEAT=1  also build release twice in separate fresh target dirs and compare digests
#
# The evidence file defaults to $AB_BUILD_ROOT/tmp/verification.txt and must be under
# $AB_BUILD_ROOT/tmp (relative paths are taken from the caller's directory). The worktree must be
# completely clean, tracked, untracked and ignored, before and after, so the log is bound to HEAD.
# Exit status is 0 only if every required check passed.
set -uo pipefail
caller_pwd="$PWD"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=ab-env.sh
. "$here/ab-env.sh" || { echo "ab-verify: environment check failed; nothing was run" >&2; exit 2; }
# shellcheck source=ab-sandbox.sh
. "$here/ab-sandbox.sh"

[ "$AB_ROOT_STATE" = new ] || { echo "ab-verify: build root $AB_BUILD_ROOT already exists; there is no reuse or continuation, choose a new AB_ID" >&2; exit 2; }

# Evidence path: canonical, under the build root's tmp/, never a symlink.
req="${1:-$AB_BUILD_ROOT/tmp/verification.txt}"
case "$req" in /*) ;; *) req="$caller_pwd/$req" ;; esac
EV="$(realpath -m -- "$req")"
case "$EV" in
  "$AB_BUILD_ROOT/tmp/"?*) ;;
  *) echo "ab-verify: evidence file must be under $AB_BUILD_ROOT/tmp (resolved '$EV')" >&2; exit 2 ;;
esac
[ ! -L "$EV" ] || { echo "ab-verify: evidence file is a symlink; refused" >&2; exit 2; }

cd "$AB_REPO" || exit 2
ab_require_clean_tree "$AB_REPO" || { echo "ab-verify: refusing to verify an unclean worktree" >&2; exit 2; }
head="$(git rev-parse HEAD)"

# --- reserve the build root atomically; mkdir without -p fails on any existing entry ------------
mkdir -m 700 -- "$AB_BUILD_ROOT" 2>/dev/null || { echo "ab-verify: could not reserve $AB_BUILD_ROOT (it appeared meanwhile or is not creatable); refused" >&2; exit 2; }
mkdir -m 700 "$AB_BUILD_ROOT"/{tmp,target,cargo,home} || exit 2
{ echo "id=$AB_ID"; echo "uid=$(id -u)"; echo "repo=$AB_REPO"; echo "head=$head"; echo "created=$(date -u +%FT%TZ)"; } >"$AB_BUILD_ROOT/.ab-root"
chmod 600 "$AB_BUILD_ROOT/.ab-root"

# Seed CARGO_HOME with the registry only (never bin/ or env), into the fresh cargo/ directory.
[ -d "$AB_REGISTRY_SEED" ] || { echo "ab-verify: registry seed missing: $AB_REGISTRY_SEED" >&2; exit 2; }
cp -a -- "$AB_REGISTRY_SEED" "$AB_BUILD_ROOT/cargo/registry" || { echo "ab-verify: registry seed copy failed" >&2; exit 2; }

ab_sandbox_build || exit 2
scratch="$AB_BUILD_ROOT/tmp"; target_dir="$AB_BUILD_ROOT/target"   # host-side names; the sandbox sets its own TMPDIR and CARGO_TARGET_DIR

lock_before="$(sha256sum Cargo.lock | cut -c1-64)"
fail=0
mkdir -p -- "$(dirname "$EV")" && : >"$EV" || { echo "ab-verify: cannot create evidence file $EV" >&2; exit 2; }
log() { printf '%s\n' "$*" | tee -a "$EV"; }

log "Asher Bridge offline verification. id=$AB_ID started=$(date -u +%FT%TZ)"
log "head=$head branch=$(git rev-parse --abbrev-ref HEAD)"
log "worktree clean (tracked, untracked and ignored): yes; build root was fresh (reserved by this run)"
log "scripts: $(sha256sum scripts/ab-env.sh scripts/ab-sandbox.sh scripts/ab-verify.sh | awk '{printf "%s=%s ", $2, substr($1,1,16)}')"
log "git ls-files -s scripts: $(git ls-files -s scripts | awk '{printf "%s:%s ", $4, substr($2,1,12)}')"
log "Cargo.lock sha256=$lock_before"
log "build root: $AB_BUILD_ROOT (mode $(stat -c %a "$AB_BUILD_ROOT"), uid $(id -u)); evidence: $EV"
log "sandbox: $(printf '%q ' "${AB_SANDBOX[@]}")"
log "free: $(df -h /var/tmp | tail -1)"

# --- D3: prove the desktop-bus boundary before any cargo command --------------------------------
log ""; log "--- boundary probe (non-notifying; runs before any cargo command) ---"
if ab_boundary_probe "${AB_SANDBOX[@]}" -- 2>&1 | tee -a "$EV"; [ "${PIPESTATUS[0]}" -ne 0 ]; then
  log "FAIL: boundary probe failed; the owner's desktop bus may be reachable. No cargo command was run."
  log "RESULT: FAIL"; exit 1
fi
log "boundary probe: PASS"

# run NAME cmd... ; runs inside the sandbox; output also kept in $scratch/<name>.out for parsing
run() {
  local name="$1"; shift
  log ""; log "\$ $*"
  "${AB_SANDBOX[@]}" -- "$@" >"$scratch/$name.out" 2>&1; local rc=$?
  cat "$scratch/$name.out" >>"$EV"
  log "exit=$rc"
  return $rc
}
need() { # name rc
  if [ "$2" -ne 0 ]; then log "FAIL: $1 exit=$2"; fail=1; fi
}
cargo="$AB_TOOLCHAIN_BIN/cargo"; rustc="$AB_TOOLCHAIN_BIN/rustc"

# 0. toolchain identity as the build will see it, from inside the sandbox
run ident sh -c 'command -v rustc; command -v cargo; command -v rustfmt; command -v cargo-clippy; rustc -vV; cargo -vV; env | LC_ALL=C sort'; need ident $?
log "--- the sandbox environment above is the complete environment of every command below ---"
[ "$(sed -n 1p "$scratch/ident.out")" = "$rustc" ] && [ "$(sed -n 2p "$scratch/ident.out")" = "$cargo" ] \
  || { log "FAIL: rustc/cargo inside the sandbox are not the pinned binaries"; fail=1; }
grep -qx 'rustc 1.99.1 (58cf8f9c3 2026-10-13)' "$scratch/ident.out" && grep -qx 'cargo 1.99.1 (5f94df478 2026-08-27)' "$scratch/ident.out" \
  || { log "FAIL: rustc -vV / cargo -vV inside the sandbox do not report the pinned versions"; fail=1; }
[ $fail -eq 0 ] || { log "STOP: toolchain identity failed; later steps not run"; log "RESULT: FAIL"; exit 1; }

# 1. dependency proof (no compilation)
run metadata "$cargo" metadata --offline --locked --format-version 1 --filter-platform x86_64-unknown-linux-gnu; rc=$?
need metadata $rc
[ "$(sha256sum Cargo.lock | cut -c1-64)" = "$lock_before" ] || { log "FAIL: Cargo.lock changed"; fail=1; }
[ $fail -eq 0 ] || { log "STOP: dependency proof failed; later steps not run"; log "RESULT: FAIL"; exit 1; }

run fmt "$cargo" fmt --check;                                                              need fmt $?
run clippy "$cargo" clippy --offline --locked --all-targets --all-features -- -D warnings; need clippy $?
run test_par "$cargo" test --offline --locked --features fault-injection;                  need test_par $?
run test_ser "$cargo" test --offline --locked --features fault-injection -- --test-threads=1; need test_ser $?
run release "$cargo" build --offline --release --locked;                                   need release $?

# 2. test accounting: exit status of every cargo command (above) and totals must be 243/0/0
count() { awk '/^test result:/ { p+=$4; f+=$6; i+=$8 } END { printf "%d %d %d", p, f, i }' "$scratch/$1.out"; }
for t in test_par test_ser; do
  read -r p f i <<<"$(count $t)"
  log ""; log "$t totals: passed=$p failed=$f ignored=$i"
  grep '^test result:' "$scratch/$t.out" | awk '{print "  " $0}' | tee -a "$EV" >/dev/null
  if [ "$p" -ne 243 ] || [ "$f" -ne 0 ] || [ "$i" -ne 0 ]; then log "FAIL: $t expected 243 passed, 0 failed, 0 ignored"; fail=1; fi
done

# 3. which compiler cargo actually used: every rustc identity cargo recorded must be the pin
log ""; log "--- compiler identity recorded by cargo in the target directory ---"
if /usr/bin/python3 -I - "$target_dir/.rustc_info.json" <<'PY' | tee -a "$EV"; [ "${PIPESTATUS[0]}" -eq 0 ]; then :; else log "FAIL: cargo's recorded compiler is not the pinned rustc"; fail=1; fi
import json, sys
pin = "rustc 1.99.1 (58cf8f9c3 2026-10-13)"
try: info = json.load(open(sys.argv[1]))
except Exception as e: print("cannot read .rustc_info.json: %s" % e); sys.exit(1)
firsts = sorted({o["stdout"].splitlines()[0] for o in info.get("outputs", {}).values() if o.get("stdout", "").startswith("rustc ")})
print("distinct compiler identities recorded by cargo: %s" % firsts)
sys.exit(0 if firsts == [pin] else 1)
PY

# 4. release checks
log ""; log "--- release binaries ---"
for b in orchestrator supervisor fake-agent; do
  f="$target_dir/release/$b"
  if [ -x "$f" ]; then
    n="$(strings -a "$f" | grep -c 'ASHER_TEST_' || true)"
    m="$(strings -a "$f" | grep -c 'before_accept_commit' || true)"
    log "$b sha256=$(sha256sum "$f" | cut -c1-64) ASHER_TEST_strings=$n barrier_marker_strings=$m"
    [ "$n" -eq 0 ] && [ "$m" -eq 0 ] || { log "FAIL: $b carries test-only strings"; fail=1; }
  else
    log "FAIL: missing $f"; fail=1
  fi
done

# 5. optional informational determinism check (not a gate)
if [ "${AB_REPEAT:-0}" = 1 ]; then
  log ""; log "--- informational: second release build in a fresh target dir ---"
  second="$AB_BUILD_ROOT/target-second"
  mkdir -m 700 "$second" || { log "skipped: cannot create $second"; second=""; }
  if [ -n "$second" ]; then
    AB_SANDBOX_SAVE=("${AB_SANDBOX[@]}")
    AB_SANDBOX+=(--bind "$second" "$second" --setenv CARGO_TARGET_DIR "$second")
    "${AB_SANDBOX[@]}" -- "$cargo" build --offline --release --locked >"$scratch/release2.out" 2>&1
    log "second build exit=$?"
    AB_SANDBOX=("${AB_SANDBOX_SAVE[@]}")
    for b in orchestrator supervisor fake-agent; do
      a="$(sha256sum "$target_dir/release/$b" | cut -c1-64)"; c="$(sha256sum "$second/release/$b" 2>/dev/null | cut -c1-64)"
      [ "$a" = "$c" ] && log "$b identical ($a)" || log "$b DIFFERS first=$a second=$c"
    done
  fi
fi

# 6. post-conditions
[ "$(sha256sum Cargo.lock | cut -c1-64)" = "$lock_before" ] || { log "FAIL: Cargo.lock changed"; fail=1; }
[ "$(git rev-parse HEAD)" = "$head" ] || { log "FAIL: HEAD moved during the run"; fail=1; }
if ab_require_clean_tree "$AB_REPO" 2>>"$EV"; then log "worktree still completely clean after the run (tracked, untracked, ignored; no target/ in the worktree)"
else log "FAIL: worktree is not clean after the run"; fail=1; fi
# A leftover is a process whose working directory or executable is inside the build root.
# (Matching command lines would also match the shell that launched this script.)
leftover=""
for d in /proc/[0-9]*; do
  pid="${d#/proc/}"; [ "$pid" = "$$" ] && continue
  c="$(readlink "$d/cwd" 2>/dev/null)"; e="$(readlink "$d/exe" 2>/dev/null)"
  case "$c$e" in *"$AB_BUILD_ROOT"*) leftover="$leftover$pid cwd=$c exe=$e"$'\n' ;; esac
done
[ -z "$leftover" ] && log "no leftover processes with cwd or exe in the build root" || { log "FAIL: leftover processes:"; log "$leftover"; fail=1; }

log ""; log "finished=$(date -u +%FT%TZ)"
if [ $fail -eq 0 ]; then log "RESULT: PASS"; exit 0; else log "RESULT: FAIL"; exit 1; fi
