# Asher Bridge verification sandbox. Source it after scripts/ab-env.sh; do not execute it.
#
#   AB_ID=<id> . scripts/ab-env.sh && . scripts/ab-sandbox.sh
#   ab_sandbox_build            # fills AB_SANDBOX
#   "${AB_SANDBOX[@]}" -- cargo test ...
#
# Every cargo, rustc and test process runs inside one bubblewrap sandbox that is built from an
# allow-list, not from the host with holes cut in it:
#
#   - Environment: --clearenv, then only the variables set below. Nothing ambient survives, so a
#     stray RUSTC, CARGO_*, CC, LD_PRELOAD or DBUS_SESSION_BUS_ADDRESS cannot reach the build.
#   - Filesystem: read-only /usr, /etc, the repository and the pinned toolchain tree; writable only
#     the build root's tmp/, target/, cargo/ and home/ (and an optional extra target directory).
#     /run, /tmp and /dev are fresh, so the owner's session bus (/run/user/<uid>/bus), the system bus,
#     tmux and agent sockets, $HOME and every other build root do not exist in the sandbox.
#   - Network: a new, empty network namespace (no IP, no abstract Unix sockets of the host).
#   - Processes: a new PID namespace; everything dies with the parent.
#   - Descriptors: stdin is /dev/null and every inherited fd >= 3 is closed before bwrap starts, so
#     no socket, directory, file or pipe held by the launcher becomes a host-access channel inside.
#
# A user + network namespace alone does NOT hide filesystem Unix sockets (the mount namespace does),
# so the mount isolation is the load-bearing part of the desktop-bus boundary and is probed by
# ab_boundary_probe before any cargo command runs. The mount namespace hides sockets reachable by
# PATH; it does nothing about a socket, directory or file descriptor the launcher already holds
# open, which bwrap passes straight through. Closing those at the boundary is the second
# load-bearing mechanism, and ab_boundary_probe deliberately inherits a synthetic socket, directory
# and pipe to prove they are stripped.

# Descriptor hygiene for every sandboxed command, run in place of the launcher just before bwrap.
# bwrap inherits the launching process's open file descriptors and hands them to the workload, so
# stdin and any socket, directory, file or pipe fd the launcher holds would otherwise be reachable
# inside the sandbox regardless of the mount isolation (which only hides paths). This trampoline
# points stdin at /dev/null and closes every descriptor >= 3, preserving only stdout and stderr (the
# required output channel), then execs bwrap. It adds no process (it execs), so --die-with-parent
# still binds bwrap to the original caller and teardown is unchanged. It fails closed: if it cannot
# enumerate descriptors or reopen stdin it refuses to launch rather than run with a dirty set. The
# glob fully expands before the loop body runs, so closing descriptors mid-iteration is safe.
_AB_FD_TRAMPOLINE='
[ -d /proc/$$/fd ] || { echo "ab-sandbox: cannot enumerate file descriptors (/proc/$$/fd); refusing to launch the sandbox" >&2; exit 127; }
exec </dev/null || { echo "ab-sandbox: cannot reopen stdin from /dev/null; refusing to launch the sandbox" >&2; exit 127; }
for __p in /proc/$$/fd/*; do
  __fd=${__p##*/}
  case "$__fd" in ""|*[!0-9]*) continue;; esac
  [ "$__fd" -ge 3 ] && eval "exec ${__fd}<&-" 2>/dev/null
done
exec "$@"
'

ab_sandbox_build() { # [extra bwrap arguments...]
  local uid user r tree p
  uid="$(id -u)"; user="$(id -un)"
  r="$AB_BUILD_ROOT"; tree="$(dirname "$AB_TOOLCHAIN_BIN")"
  [ -x /usr/bin/bwrap ] || { echo "ab-sandbox: /usr/bin/bwrap is required and missing; refusing to run unisolated" >&2; return 1; }
  [ -x /usr/bin/bash ] || { echo "ab-sandbox: /usr/bin/bash is required for descriptor hygiene and is missing; refusing to run" >&2; return 1; }
  [ -c /dev/null ] || { echo "ab-sandbox: /dev/null is not a usable character device; refusing to run without a safe stdin" >&2; return 1; }
  AB_SANDBOX_RAW=(/usr/bin/bwrap
    --unshare-user --unshare-net --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup-try
    --die-with-parent --new-session --clearenv
    --ro-bind /usr /usr)
  for p in /bin /sbin /lib /lib64; do   # reproduce the host's usr-merge layout
    if [ -L "$p" ]; then AB_SANDBOX_RAW+=(--symlink "$(readlink "$p")" "$p")
    elif [ -d "$p" ]; then AB_SANDBOX_RAW+=(--ro-bind "$p" "$p"); fi
  done
  AB_SANDBOX_RAW+=(--ro-bind /etc /etc --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run
    --ro-bind "$AB_REPO" "$AB_REPO" --ro-bind "$tree" "$tree"
    --bind "$r/tmp" "$r/tmp" --bind "$r/target" "$r/target" --bind "$r/cargo" "$r/cargo" --bind "$r/home" "$r/home"
    --chdir "$AB_REPO"
    --setenv PATH "$AB_TOOLCHAIN_BIN:/usr/bin:/bin"
    --setenv HOME "$r/home" --setenv USER "$user" --setenv LOGNAME "$user"
    --setenv LANG C.UTF-8 --setenv TERM dumb
    --setenv CARGO_HOME "$r/cargo" --setenv CARGO_TARGET_DIR "$r/target" --setenv TMPDIR "$r/tmp"
    --setenv CARGO_NET_OFFLINE true --setenv CARGO_TERM_COLOR never
    --setenv RUSTC "$AB_TOOLCHAIN_BIN/rustc" --setenv RUSTDOC "$AB_TOOLCHAIN_BIN/rustdoc" --setenv RUSTFMT "$AB_TOOLCHAIN_BIN/rustfmt"
    # Defence in depth only: a dead path, so the product fails closed (src/dbus.rs). The mount
    # isolation above is the mechanism; this is not.
    --setenv DBUS_SESSION_BUS_ADDRESS "unix:path=$r/no-bus"
    "$@")
  # AB_SANDBOX_RAW is the mount/namespace isolation only; it still inherits the launcher's fds.
  # The supported launch path is AB_SANDBOX: the fd-hygiene trampoline in front of it. ab-verify.sh
  # and ab_boundary_probe use AB_SANDBOX; ab-selftest.sh uses AB_SANDBOX_RAW as a negative control to
  # prove the fd probe fails without the trampoline.
  AB_SANDBOX=(/usr/bin/bash --noprofile --norc -c "$_AB_FD_TRAMPOLINE" ab-sandbox "${AB_SANDBOX_RAW[@]}")
}

# The Python used inside and outside the sandbox. Standard library only, no bytes are ever sent.
_AB_PY_DECOY='
import socket, sys, os
path, log, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(5); s.settimeout(secs)
open(log, "a").write("listening\n")
try:
    while True:
        c, _ = s.accept(); open(log, "a").write("ACCEPTED\n"); c.close()
except socket.timeout:
    pass
'
_AB_PY_CONNECT='
import socket, sys, errno
s = socket.socket(socket.AF_UNIX); s.settimeout(3)
try:
    s.connect(sys.argv[1]); print("CONNECTED")
except OSError as e:
    print("refused:" + errno.errorcode.get(e.errno, str(e.errno)))
'
# Runs INSIDE the wrapper under test. argv: real_bus system_bus decoy tmux_glob_dir uid
_AB_PY_INSIDE='
import os, sys, socket, errno, stat
real_bus, sys_bus, decoy, tmpdir, uid = sys.argv[1:6]
bad = 0
def say(ok, msg):
    global bad
    print(("  ok   " if ok else "  FAIL ") + msg)
    if not ok: bad = 1
# Inherited file descriptors, snapshotted FIRST, before this probe opens any socket of its own, so
# the set is exactly what the launcher handed in. bwrap passes the launcher s open fds straight
# through, so a socket, directory, file or pipe held outside the sandbox would be a host-access
# channel here that the mount isolation cannot close. The supported launch path (the fd trampoline)
# must leave only stdin/stdout/stderr, with stdin neutralized. Enumerate without the measurement
# artifact that os.listdir(dir_fd) leaves (it dups the dir fd): keep only descriptors whose link
# still resolves.
def _fdkind(m):
    return ("socket" if stat.S_ISSOCK(m) else "dir" if stat.S_ISDIR(m) else "fifo" if stat.S_ISFIFO(m)
            else "chr" if stat.S_ISCHR(m) else "file" if stat.S_ISREG(m) else "blk" if stat.S_ISBLK(m) else "other")
_fdd = os.open("/proc/self/fd", os.O_RDONLY | os.O_DIRECTORY)
_ent = [int(e) for e in os.listdir(_fdd)]
os.close(_fdd)
_fds = {}
for _f in _ent:
    try: _link = os.readlink("/proc/self/fd/%d" % _f)   # ENOENT => enumerator artifact, already closed
    except OSError: continue
    try: _fds[_f] = (_link, os.fstat(_f).st_mode)
    except OSError: continue
_extra = sorted(_f for _f in _fds if _f >= 3)
say(not _extra, "no inherited file descriptor >= 3 (%s)" % (["%d:%s:%s" % (_f, _fdkind(_fds[_f][1]), _fds[_f][0]) for _f in _extra] or "none"))
_m0 = _fds.get(0, (None, 0))[1]
say(0 in _fds and not (stat.S_ISSOCK(_m0) or stat.S_ISFIFO(_m0) or stat.S_ISDIR(_m0)),
    "fd 0 (stdin) is not a socket, pipe or directory: %s" % (_fds.get(0, ("absent",))[0]))
_chan = sorted(_f for _f, (_lnk, _md) in _fds.items() if stat.S_ISSOCK(_md) or stat.S_ISDIR(_md))
say(not _chan, "no inherited socket or directory descriptor on any fd (%s)" % (["%d:%s:%s" % (_f, _fdkind(_fds[_f][1]), _fds[_f][0]) for _f in _chan] or "none"))
def absent(p):
    try:
        st = os.lstat(p); return False, stat.S_IFMT(st.st_mode) == stat.S_IFSOCK
    except FileNotFoundError: return True, False
    except OSError as e: return True, False
for p in (real_bus, sys_bus, os.path.dirname(real_bus), tmpdir):
    gone, sock = absent(p)
    say(gone, "lstat %s -> %s" % (p, "ENOENT" if gone else ("SOCKET present" if sock else "present")))
# Connect-only attempts, and only where lstat already says the path is absent: a leaking wrapper is
# reported by the lstat above and never connected to. No byte is ever sent on any socket.
for p in (real_bus, decoy):
    if absent(p)[0]:
        s = socket.socket(socket.AF_UNIX); s.settimeout(3)
        try: s.connect(p); say(False, "connect %s unexpectedly CONNECTED" % p)
        except OSError as e: say(e.errno == errno.ENOENT, "connect %s -> %s" % (p, errno.errorcode.get(e.errno, e.errno)))
    elif p == decoy:
        s = socket.socket(socket.AF_UNIX); s.settimeout(3)
        try: s.connect(p); say(False, "connect %s CONNECTED (the wrapper does not hide filesystem sockets)" % p)
        except OSError as e: say(False, "decoy visible but connect failed: %s" % e)
say(not os.path.exists("/run/user"), "/run/user does not exist")
say(os.environ.get("XDG_RUNTIME_DIR") is None, "XDG_RUNTIME_DIR is unset")
addr = os.environ.get("DBUS_SESSION_BUS_ADDRESS")
say(addr is None or (addr.startswith("unix:path=") and addr != "unix:path=" + real_bus), "DBUS_SESSION_BUS_ADDRESS=%s is not the real bus" % addr)
t = socket.socket(); t.settimeout(3)
try: t.connect(("1.1.1.1", 443)); say(False, "TCP to 1.1.1.1:443 connected")
except OSError as e: say(e.errno in (errno.ENETUNREACH, errno.EHOSTUNREACH, errno.ECONNREFUSED), "TCP to 1.1.1.1:443 -> %s" % errno.errorcode.get(e.errno, e.errno))
try: n = [l for l in open("/proc/net/unix").read().splitlines()[1:] if "@" in l.split()[-1]]
except OSError: n = []
say(not n, "no host abstract Unix sockets visible (%d)" % len(n))
sys.exit(bad)
'

# Launch the inside check with synthetic descriptors deliberately inherited, then exec the wrapper
# under test. argv: synthetic-dir  WRAPPER... -- cmd...  . It makes a socketpair, opens a directory
# fd on the synthetic dir (a host-only path not bound into the sandbox) and a pipe, places them at
# fds 3, 4 and 5 (inheritable), and execs the rest. The supported sandbox must strip all three; a
# wrapper without fd hygiene leaks them and the inside check reports it. No byte is ever sent on the
# socketpair and the directory is only opened, never read, so this notifies nothing.
_AB_PY_WITHFDS='
import os, socket, sys
d = sys.argv[1]; argv = sys.argv[2:]
a, b = socket.socketpair()
dfd = os.open(d, os.O_RDONLY | os.O_DIRECTORY)
pr, pw = os.pipe()
for n, f in ((3, b.fileno()), (4, dfd), (5, pw)):
    os.dup2(f, n); os.set_inheritable(n, True)
os.execv(argv[0], argv)
'

# ab_boundary_probe WRAPPER... : prove that nothing inside WRAPPER can reach the owner's session bus
# and that WRAPPER strips inherited descriptors. WRAPPER is the command prefix that runs a command in
# the isolation under test, ending in "--" (for the real sandbox: "${AB_SANDBOX[@]}" --).
# Non-notifying by construction: the real bus is only lstat-ed, and connected to only if lstat
# already reports it absent; a live decoy listener in the build root stands in for a bus; the
# deliberately inherited socket is an unconnected socketpair; no byte is ever written to any socket.
ab_boundary_probe() {
  local uid real_bus sys_bus d log pid i n rc=0 out
  uid="$(id -u)"; real_bus="/run/user/$uid/bus"; sys_bus=/run/dbus/system_bus_socket
  d="$AB_BUILD_ROOT/decoy"; log="$d/accept.log"
  mkdir -p "$d" || return 1
  rm -f "$d/probe.sock" "$log"
  if [ -S "$real_bus" ]; then echo "probe: control: the real session bus socket exists on the host ($real_bus), checked with stat only; never connected from outside"
  else echo "probe: control: no real session bus socket on this host; the real-path checks below are weaker, the decoy checks still apply"; fi
  /usr/bin/python3 -I -c "$_AB_PY_DECOY" "$d/probe.sock" "$log" 20 & pid=$!
  for i in 1 2 3 4 5 6 7 8 9 10; do [ -S "$d/probe.sock" ] && grep -qx listening "$log" 2>/dev/null && break; sleep 0.3; done
  out="$(/usr/bin/python3 -I -c "$_AB_PY_CONNECT" "$d/probe.sock")"
  n="$(grep -c ACCEPTED "$log" 2>/dev/null)"
  if [ "$out" = CONNECTED ] && [ "$n" = 1 ]; then echo "probe: control: decoy listener accepts a connection from outside the isolation (count=$n)"
  else echo "probe: FAIL: decoy control did not work (out=$out accepted=$n); probe is vacuous" >&2; rc=1; fi
  echo "probe: inside the isolation (launched with a synthetic socket, directory and pipe deliberately inherited):"
  /usr/bin/python3 -I -c "$_AB_PY_WITHFDS" "$d" \
    "$@" /usr/bin/python3 -I -c "$_AB_PY_INSIDE" "$real_bus" "$sys_bus" "$d/probe.sock" "/tmp/tmux-$uid" "$uid" || rc=1
  n="$(grep -c ACCEPTED "$log" 2>/dev/null)"
  if [ "$n" = 1 ]; then echo "probe: decoy listener saw no connection from inside the isolation (total accepted=$n, the one control connection)"
  else echo "probe: FAIL: decoy listener accepted $n connections; an in-isolation connection reached it" >&2; rc=1; fi
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  rm -f "$d/probe.sock"
  return $rc
}
