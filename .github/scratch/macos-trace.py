#!/usr/bin/env python3
"""Scratch: collect, on a hosted macOS runner, what a read base set is derived from.

Phases (each writes its raw output under the output directory):
  facts      OS and Xcode facts, the catalog questions of js-mac-pool::xcode
  setuid     setuid/setgid files, as the runner user and as root
  fsusage    paths the reference workloads touch (fs_usage, unsandboxed control run)
  minimize   the reference workloads under an allow profile, then each candidate line
             removed in turn: a line is needed only when a workload fails without it
"""
import os
import subprocess
import sys
import time

OUT = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith("--") else None

# The macOS 14 base set of js-mac-pool, as the starting candidates (not as an answer).
NAMES14 = ["/System", "/etc", "/tmp", "/usr", "/var"]
READS14 = [
    ("literal", "/"),
    ("subpath", "/System/Library"),
    ("subpath", "/System/Cryptexes"),
    ("subpath", "/System/Volumes/Preboot/Cryptexes"),
    ("subpath", "/usr/lib"),
    ("subpath", "/usr/bin"),
    ("subpath", "/usr/sbin"),
    ("subpath", "/usr/libexec"),
    ("subpath", "/usr/share"),
    ("subpath", "/bin"),
    ("subpath", "/sbin"),
    ("subpath", "/private/var/db/dyld"),
    ("subpath", "/private/var/db/timezone"),
    ("literal", "/private/var/select/sh"),
    ("literal", "/dev/null"),
    ("literal", "/dev/zero"),
    ("literal", "/dev/random"),
    ("literal", "/dev/urandom"),
    ("literal", "/dev/tty"),
]
SETUID14 = [
    "/System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/MacOS/ARDAgent",
    "/bin/ps", "/sbin/ping", "/usr/bin/at", "/usr/bin/atq", "/usr/bin/atrm", "/usr/bin/batch",
    "/usr/bin/crontab", "/usr/bin/login", "/usr/bin/newgrp", "/usr/bin/quota", "/usr/bin/su",
    "/usr/bin/sudo", "/usr/bin/top", "/usr/bin/write", "/usr/libexec/authopen",
    "/usr/libexec/security_authtrampoline", "/usr/sbin/postdrop", "/usr/sbin/postqueue",
    "/usr/sbin/traceroute", "/usr/sbin/traceroute6",
]
KIND_TIMEZONE = [("literal", "/private/etc/localtime"), ("subpath", "/usr/share/zoneinfo")]
XCODE_SUPPORT = [("literal", "/Library/Preferences/com.apple.dt.Xcode.plist")]
PRIV_BITS = 0o6000


def say(text=""):
    print(text, flush=True)


def sh(argv, env=None, cwd=None, timeout=300):
    try:
        p = subprocess.run(argv, capture_output=True, text=True, env=env, cwd=cwd, timeout=timeout)
        return p.returncode, p.stdout, p.stderr
    except Exception as e:  # noqa: BLE001
        return 99, "", f"{type(e).__name__}: {e}"


def save(name, text):
    with open(os.path.join(OUT, name), "w") as f:
        f.write(text)


def scan(roots, xdev=False):
    found = []
    for root in roots:
        try:
            dev = os.lstat(root).st_dev
        except OSError:
            continue
        stack = [root]
        while stack:
            p = stack.pop()
            try:
                st = os.lstat(p)
            except OSError:
                continue
            if os.path.isdir(p) and not os.path.islink(p):
                if xdev and st.st_dev != dev:
                    continue
                try:
                    for e in os.scandir(p):
                        stack.append(e.path)
                except OSError:
                    pass
            elif os.path.isfile(p) and not os.path.islink(p) and st.st_mode & PRIV_BITS:
                found.append(p)
    return sorted(set(found))


if len(sys.argv) > 1 and sys.argv[1] == "--scan":
    xdev = sys.argv[2] == "xdev"
    for p in scan(sys.argv[3:], xdev):
        print(p)
    sys.exit(0)

os.makedirs(OUT, exist_ok=True)
HERE = os.path.abspath(__file__)

# ---------------------------------------------------------------- facts
facts = []
def fact(title, argv, env=None):
    rc, out, err = sh(argv, env=env)
    facts.append(f"$ {' '.join(argv)}  (env={env if env is not None else 'inherit'})\nexit={rc}\n{out}{err}\n")
    return rc, out, err

fact("sw_vers", ["/usr/bin/sw_vers"])
fact("uname", ["/usr/bin/uname", "-a"])
fact("id", ["/usr/bin/id"])
fact("csrutil", ["/usr/bin/csrutil", "status"])
fact("xcode-select", ["/usr/bin/xcode-select", "-p"])
fact("ls apps", ["/bin/ls", "-ld", "/Applications"] + sorted(
    os.path.join("/Applications", n) for n in os.listdir("/Applications") if n.startswith("Xcode")))
fact("sudo", ["sudo", "-n", "true"])
APP = "/Applications/Xcode.app"
if not os.path.exists(APP):
    d = sh(["/usr/bin/xcode-select", "-p"])[1].strip()
    APP = d[: -len("/Contents/Developer")] if d.endswith("/Contents/Developer") else d
REAL_APP = os.path.realpath(APP)
facts.append(f"APP={APP} REAL_APP={REAL_APP}\n")
dev = {"DEVELOPER_DIR": f"{APP}/Contents/Developer"}
fact("xcodebuild -version", [f"{APP}/Contents/Developer/usr/bin/xcodebuild", "-version"], env=dev)
fact("xcodebuild -license check", [f"{APP}/Contents/Developer/usr/bin/xcodebuild", "-license", "check"], env=dev)
fact("xcodebuild -checkFirstLaunchStatus", [f"{APP}/Contents/Developer/usr/bin/xcodebuild", "-checkFirstLaunchStatus"], env=dev)
fact("xcrun --find clang", ["/usr/bin/xcrun", "--find", "clang"], env=dev)
fact("stat app chain", ["/usr/bin/stat", "-f", "%N owner=%Su group=%Sg mode=%Sp", APP, REAL_APP, "/Applications", "/"])
fact("xcode plist", ["/bin/ls", "-l", "/Library/Preferences/com.apple.dt.Xcode.plist"])
fact("sandbox sw", ["/usr/bin/sandbox-exec", "-n", "no-write", "/usr/bin/true"])
save("facts.txt", "\n".join(facts))

# ---------------------------------------------------------------- setuid
ROOTS14 = [p for k, p in READS14 if k == "subpath"]
user_found = scan(ROOTS14)
rc, out, err = sh(["sudo", "-n", "python3", HERE, "--scan", "plain"] + ROOTS14)
root_found = sorted(set(out.split("\n")) - {""})
wide_roots = ["/", "/System/Volumes/Preboot/Cryptexes", "/Library", "/private/var/db", "/private/etc", "/private/var/select"]
rc2, out2, err2 = sh(["sudo", "-n", "python3", HERE, "--scan", "xdev"] + wide_roots, timeout=900)
wide_found = sorted(set(out2.split("\n")) - {""})
save("setuid.txt",
     "# as runner user over the macOS 14 subpath roots\n" + "\n".join(user_found) +
     f"\n# as root over the same roots (rc={rc} err={err[:200]!r})\n" + "\n".join(root_found) +
     f"\n# as root, wider roots {wide_roots} xdev (rc={rc2} err={err2[:200]!r})\n" + "\n".join(wide_found) + "\n")
say(f"setuid: user={len(user_found)} root={len(root_found)} wide={len(wide_found)}")

# ---------------------------------------------------------------- fixture
WORK = os.path.join(os.environ.get("RUNNER_TEMP", "/tmp"), "trace-work")
RUN = os.path.join(WORK, "run")
TREE = os.path.join(WORK, "store", "tree-a")
OTHER = os.path.join(WORK, "other")
for d in (RUN, TREE + "/bin", OTHER, os.path.join(WORK, "home-other"), os.path.join(WORK, "inputs"), os.path.join(WORK, "sysdata")):
    os.makedirs(d, exist_ok=True)
DATA = "fixture-data\n"
open(os.path.join(TREE, "data.txt"), "w").write(DATA)
open(os.path.join(TREE, "bin/read"), "w").write('#!/bin/sh\nexec /bin/cat "$@"\n')
os.chmod(os.path.join(TREE, "bin/read"), 0o755)
open(os.path.join(RUN, "stub.c"), "w").write("int main(void) { return 0; }\n")
canary_input = os.path.join(WORK, "inputs", "secret.txt")
canary_home = os.path.join(WORK, "home-other", "secret.txt")
canary_data = "/System/Volumes/Data" + os.path.join(WORK, "sysdata", "secret.txt")
canary_shared = f"/Users/Shared/trace-{os.getpid()}"
canary_vartmp = f"/private/var/tmp/trace-{os.getpid()}"
for p in (canary_input, canary_home, os.path.join(WORK, "sysdata", "secret.txt"), canary_shared, canary_vartmp):
    open(p, "w").write("secret\n")
temp = sh(["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"])[1].strip()
UENTRY = os.path.dirname(os.path.realpath(temp))
WORK = os.path.realpath(WORK)  # profiles match resolved paths
RUN = os.path.realpath(RUN)
TREE = os.path.realpath(TREE)
canary_input = os.path.realpath(canary_input)
canary_home = os.path.realpath(canary_home)
canary_data = "/System/Volumes/Data" + os.path.realpath(os.path.join(WORK, "sysdata", "secret.txt"))
xdev_env = [("DEVELOPER_DIR", os.path.realpath(f"{APP}/Contents/Developer")), ("HOME", RUN), ("TMPDIR", RUN)]
# The real Xcode path the fixture grants is APP (not its target); grant both when they differ.
APPS = sorted({APP, REAL_APP})

# name, argv, env, expectation
def workloads():
    cat = "/bin/cat"
    w = [
        ("allowed:true", ["/usr/bin/true"], [], "ok"),
        ("allowed:cat-granted-tree", [cat, os.path.join(TREE, "data.txt")], [], "ok"),
        ("allowed:bundle-entrypoint", [os.path.join(TREE, "bin/read"), os.path.join(TREE, "data.txt")], [], "data"),
        ("allowed:cc-compile", ["/usr/bin/cc", os.path.join(RUN, "stub.c"), "-o", os.path.join(RUN, "stub.out")], xdev_env, "ok"),
        ("allowed:codesign", ["/usr/bin/codesign", "-s", "-", "-f", os.path.join(RUN, "stub.out")], xdev_env, "ok"),
        ("allowed:tz-localtime", [cat, "/private/etc/localtime"], [], "ok"),
    ]
    for label, path in (("input-store", canary_input), ("other-home", canary_home), ("users-shared", canary_shared),
                        ("private-var-tmp", canary_vartmp), ("system-volumes-data", canary_data),
                        ("keychain", "/Library/Keychains/System.keychain"), ("setuid-su", "/usr/bin/su")):
        w.append((f"denied:cat-{label}", [cat, path], [], "refused"))
    w.append(("denied:ls-denied-dir", ["/bin/ls", os.path.dirname(canary_home)], [], "refused"))
    w.append(("denied:ls-ancestor", ["/bin/ls", os.path.dirname(RUN)], [], "refused"))
    w.append(("denied:stat-canary", ["/usr/bin/stat", canary_input], [], "refused"))
    w.append(("denied:xattr-canary", ["/usr/bin/xattr", "-l", canary_input], [], "refused"))
    return w


def refusal(stderr):
    for line in stderr.splitlines():
        line = line.strip()
        own = not line.startswith("sandbox-exec:")
        tool = line.endswith(": Operation not permitted") and ": " in line[: -len(": Operation not permitted")]
        errno = "[Errno 1] Operation not permitted" in line
        if own and (tool or errno):
            return True
    return False


def profile(names, reads, setuid):
    def f(r):
        return f'({r[0]} "{r[1]}")'
    L = [
        "(version 1)", "(allow default)", "(deny file-write*)", "(deny job-creation)", "(deny lsopen)",
        "(deny user-preference-write)",
        f'(allow file-write* (subpath "{RUN}") (subpath "{UENTRY}") (literal "/dev/null") (subpath "/dev/fd"))',
        "(deny file-read*)",
    ]
    anc = [RUN, UENTRY, TREE] + APPS
    L.append("(allow file-read-metadata " + " ".join(f'(path-ancestors "{a}")' for a in anc) + ")")
    if names:
        L.append("(allow file-read-metadata " + " ".join(f'(literal "{n}")' for n in names) + ")")
    L.append("(allow file-read* " + " ".join(f(r) for r in reads) + ")")
    L.append(f'(allow file-read* (subpath "{RUN}") (subpath "{UENTRY}"))')
    L.append(f'(allow file-read* (subpath "{TREE}"))')
    L.append("(allow file-read* " + " ".join(f'(subpath "{a}")' for a in APPS) + " " + " ".join(f(r) for r in XCODE_SUPPORT) + ")")
    L.append("(allow file-read* " + " ".join(f(r) for r in KIND_TIMEZONE) + ")")
    if setuid:
        L.append("(deny file-read* " + " ".join(f'(literal "{p}")' for p in setuid) + ")")
    L += ["(deny network*)", "(deny mach-register)", "(deny file-mount)"]
    return "\n".join(L)


def run_one(w, prof, sandboxed=True):
    name, argv, env, expect = w
    e = dict(env)
    cmd = (["/usr/bin/sandbox-exec", "-p", prof] if sandboxed else []) + argv
    for stale in ("stub.out",):
        pass
    rc, out, err = sh(cmd, env=e, cwd=RUN, timeout=120)
    if expect == "ok":
        ok = rc == 0
    elif expect == "data":
        ok = rc == 0 and out == DATA
    else:
        ok = rc != 0 and refusal(err)
    return ok, rc, out, err


def run_all(prof, only=None):
    res = {}
    for w in workloads():
        if only and not w[0].startswith(only):
            continue
        if w[0] == "allowed:codesign" and not os.path.exists(os.path.join(RUN, "stub.out")):
            pass
        res[w[0]] = run_one(w, prof)
    return res


# ---------------------------------------------------------------- controls
ctrl = {}
run_one(("allowed:cc-compile", ["/usr/bin/cc", os.path.join(RUN, "stub.c"), "-o", os.path.join(RUN, "stub.out")], xdev_env, "ok"), "", sandboxed=False)
for w in workloads():
    if w[0].startswith("denied:"):
        ok, rc, out, err = run_one((w[0], w[1], w[2], "ok"), "", sandboxed=False)
        ctrl[w[0]] = rc
    else:
        ok, rc, out, err = run_one(w, "", sandboxed=False)
        ctrl[w[0]] = rc
save("controls.txt", "\n".join(f"{k} control_exit={v}" for k, v in ctrl.items()) + "\n")

# ---------------------------------------------------------------- fs_usage
touched = {}
fsu_path = os.path.join(OUT, "fs_usage.raw")
fsf = open(fsu_path, "w")
fsp = subprocess.Popen(["sudo", "-n", "fs_usage", "-w", "-f", "filesys"], stdout=fsf, stderr=subprocess.STDOUT)
time.sleep(4)
for w in workloads():
    argv = w[1]
    sh(argv, env=dict(w[2]), cwd=RUN, timeout=120)
time.sleep(3)
sh(["sudo", "-n", "pkill", "-f", "fs_usage"])
try:
    fsp.wait(timeout=20)
except Exception:  # noqa: BLE001
    fsp.kill()
fsf.close()
WL_PROCS = ("true", "cat", "read", "sh", "cc", "xcrun", "clang", "ld", "codesign", "ls", "stat", "xattr",
            "python3", "Python", "bash", "xcodebuild", "dyld", "libtool", "as", "swift", "git")
raw = open(fsu_path, errors="replace").read().splitlines()
lines_kept = 0
for line in raw:
    parts = line.rstrip().rsplit(None, 1)
    if len(parts) != 2:
        continue
    proc = parts[1].rsplit(".", 1)[0] if "." in parts[1] else parts[1]
    if proc not in WL_PROCS:
        continue
    lines_kept += 1
    for tok in line.split():
        if tok.startswith("/") and len(tok) > 1 and not tok.startswith("/dev/fd"):
            touched.setdefault(tok, set()).add(proc)
save("fs_usage_touched.txt", "\n".join(f"{p}\t{','.join(sorted(v))}" for p, v in sorted(touched.items())) + "\n")
say(f"fs_usage: raw={len(raw)} kept={lines_kept} paths={len(touched)}")

# ---------------------------------------------------------------- minimize
def covered(path, items):
    for kind, p in items:
        if kind == "literal" and path == p:
            return True
        if kind == "subpath" and (path == p or path.startswith(p + "/")):
            return True
    return False

granted = [("subpath", x) for x in [RUN, UENTRY, TREE, WORK] + APPS] + KIND_TIMEZONE + XCODE_SUPPORT
uncovered = sorted(p for p in touched if not covered(p, READS14) and not covered(p, granted))
save("uncovered_vs_14_and_grants.txt", "\n".join(f"{p}\t{','.join(sorted(touched[p]))}" for p in uncovered) + "\n")
say(f"uncovered touched paths: {len(uncovered)}")
tops = sorted({"/" + p.split("/")[1] for p in uncovered if p.count("/") >= 1} | set(NAMES14))

setuid_deny = sorted(set(root_found) | set(user_found))
reads = list(READS14)
names = list(NAMES14)
# Candidate additions from the trace: each uncovered touched path as a literal (capped).
extra = [("literal", p) for p in uncovered if not p.startswith(("/dev/fd",))][:120]
log = []
def logline(s):
    log.append(s)
    say(s)

full = profile(names, reads + extra, setuid_deny)
base_res = run_all(full)
logline("== 14 base + trace extras: " + ", ".join(f"{k}={'ok' if v[0] else 'FAIL(rc=%s)' % v[1]}" for k, v in base_res.items()))
for k, v in base_res.items():
    if not v[0]:
        logline(f"   {k}: rc={v[1]} stdout={v[2][:200]!r} stderr={v[3][:400]!r}")
bare14 = profile(names, reads, setuid_deny)
res14 = run_all(bare14)
logline("== 14 base alone: " + ", ".join(f"{k}={'ok' if v[0] else 'FAIL(rc=%s)' % v[1]}" for k, v in res14.items()))
for k, v in res14.items():
    if not v[0]:
        logline(f"   {k}: rc={v[1]} stdout={v[2][:200]!r} stderr={v[3][:400]!r}")

# Each line removed in turn from (14 base + extras): the workloads that fail without it.
needed = {}
base_alone_ok = all(v[0] for v in res14.values())
cands = [("name", n) for n in names] + [("read",) + r for r in reads]
if not base_alone_ok:
    cands += [("extra",) + r for r in extra]
for c in cands:
    n2 = [n for n in names if not (c[0] == "name" and n == c[1])]
    r2 = [r for r in reads if not (c[0] == "read" and r == c[1:])]
    e2 = [r for r in extra if not (c[0] == "extra" and r == c[1:])]
    res = run_all(profile(n2, r2 + e2, setuid_deny))
    fails = sorted(k for k, v in res.items() if not v[0])
    needed[c] = fails
    logline(f"remove {c}: {'needed by ' + ','.join(fails) if fails else 'not needed'}")
    if fails:
        k = fails[0]
        logline(f"    e.g. {k}: rc={res[k][1]} stderr={res[k][3][:300]!r}")
save("minimize.txt", "\n".join(log) + "\n")
say("done")
