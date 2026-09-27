#!/usr/bin/env python3
"""FHS install smoke: run install.sh inside disposable containers.

Fixed image tags from the operator-pulled local cache (never
--pull always): debian:12, fedora:36, archlinux:latest. Staged rollout:
the default run is debian:12 only; --all runs every tier after the
Debian tier has passed.

Checks per tier (1-3 in the container as a normal user):
  1. install: every payload binary is linked by name into ~/.local/bin
  2. upgrade idempotence: same-version rerun, then a second version
     directory takes over and the old one is cleaned up
  3. uninstall cleanliness: payload and links gone; a sentinel file in
     the data root ~/.local/share/kallipai survives the uninstall

Host-side checks:
  4. NEEDED inventory over the tarball binaries (glibc family only)
  5. --with-systemd tier: units rendered in the container, then
     systemd-analyze verify --root on the host against the rendered
     copies (with stub executables for the container-side paths)
  6. negative paths: check_arch / check_glibc sourced from install.sh
     and called with forged inputs

Usage: python3 scripts/fhs-smoke.py --tarball <kallipai-VER-linux-x86_64.tar.gz> [--all]
"""

import argparse
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

IMAGES = ["debian:12", "fedora:36", "archlinux:latest"]

results: list[tuple[str, bool, str]] = []


def record(name: str, ok: bool, detail: str = "") -> bool:
    results.append((name, ok, detail))
    print(
        f"  [{'PASS' if ok else 'FAIL'}] {name}"
        + (f": {detail}" if detail and not ok else "")
    )
    return ok


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    print(f"  $ {' '.join(cmd[:6])}{' ...' if len(cmd) > 6 else ''}")
    # Deliberate: the helper returns the process for the caller to judge.
    return subprocess.run(cmd, capture_output=True, text=True, **kw)  # noqa: PLW1510


def must_run(cmd: list[str], **kw) -> str:
    proc = run(cmd, **kw)
    if proc.returncode != 0:
        print(proc.stdout)
        print(proc.stderr, file=sys.stderr)
        sys.exit(f"smoke: command failed ({proc.returncode}): {' '.join(cmd[:3])}...")
    return proc.stdout


# ---------------------------------------------------------------- host checks


def check_needed(tarball: Path) -> None:
    """NEEDED inventory over the payload binaries: glibc family only."""
    # Regression sentinel: the build bundles rustls, so a libssl/libcrypto
    # entry would mean it silently regressed to dynamic OpenSSL.
    allowed = {"libc.so.6", "libm.so.6", "libgcc_s.so.1", "ld-linux-x86-64.so.2"}
    found: set[str] = set()
    with tempfile.TemporaryDirectory() as td:
        with tarfile.open(tarball) as tf:
            tf.extractall(td)
        bins = sorted((Path(td) / "bin").iterdir())
        for binary in bins:
            out = must_run(["objdump", "-p", str(binary)])
            for line in out.splitlines():
                line = line.strip()
                if line.startswith("NEEDED"):
                    found.add(line.split()[1])
    bad = found - allowed
    record(
        "needed-inventory",
        not bad,
        f"unexpected: {sorted(bad)}; full set: {sorted(found)}"
        if bad
        else f"{len(found)} entries, all allowed",
    )


def check_negative_paths(repo: Path) -> None:
    """Forged-input runs of the factored gates inside install.sh."""
    cases = [
        (["check_arch"], 0),
        (["check_arch", "aarch64"], 1),
        (["check_glibc", "2.31"], 1),
        (["check_glibc", "2.35"], 0),
        (["check_glibc", "2.42"], 0),
    ]
    all_ok = True
    for calls, expected in cases:
        proc = run(
            ["bash", "-c", "set -u; source ./install.sh; " + " ".join(calls)],
            cwd=repo,
            env={**os.environ, "KALLIPAI_INSTALL_SKIP_MAIN": "1"},
        )
        all_ok &= record(
            f"negative-path {' '.join(calls)} -> rc {expected}",
            proc.returncode == expected,
            (proc.stderr or proc.stdout).strip().splitlines()[-1]
            if proc.returncode != expected
            else "",
        )
    record("negative-paths", all_ok)


# ------------------------------------------------------------ container tiers

USER_SCRIPT = r"""set -eu
NAME=__TARBALL_NAME__
useradd -m smoke
su smoke -c 'bash /mnt/install.sh --tarball /mnt/'"$NAME"
# 1. every payload binary is linked by name into ~/.local/bin
for f in /home/smoke/.local/lib/kallipai/*/bin/*; do
  n="$(basename "$f")"
  test -L "/home/smoke/.local/bin/$n"
  test "$(readlink -f "/home/smoke/.local/bin/$n")" = "$(readlink -f "$f")"
done
test -x /home/smoke/.local/bin/kallipctl
# 2a. same-version rerun stays idempotent
su smoke -c 'bash /mnt/install.sh --tarball /mnt/'"$NAME"
test "$(ls /home/smoke/.local/lib/kallipai | wc -l)" -eq 1
# 2b. a second version takes over and the old directory is cleaned up
mkdir -p /tmp/v2 && cp "/mnt/$NAME" /tmp/v2/kallipai-2.0.0-linux-x86_64.tar.gz
(cd /tmp/v2 && sha256sum kallipai-2.0.0-linux-x86_64.tar.gz > kallipai-2.0.0-linux-x86_64.tar.gz.sha256)
su smoke -c 'bash /mnt/install.sh --tarball /tmp/v2/kallipai-2.0.0-linux-x86_64.tar.gz'
test "$(ls /home/smoke/.local/lib/kallipai | wc -l)" -eq 1
readlink /home/smoke/.local/bin/kallipctl | grep -q 2.0.0
# 3. uninstall is clean; the data root survives with its sentinel
mkdir -p /home/smoke/.local/share/kallipai
echo smoke-sentinel > /home/smoke/.local/share/kallipai/sentinel
su smoke -c 'bash /mnt/install.sh --uninstall'
test ! -e /home/smoke/.local/lib/kallipai
test -z "$(ls /home/smoke/.local/bin 2>/dev/null)"
grep -q smoke-sentinel /home/smoke/.local/share/kallipai/sentinel
"""

SYSTEM_SCRIPT = r"""set -eu
NAME=__TARBALL_NAME__
bash /mnt/install.sh --with-systemd --tarball "/mnt/$NAME"
test -d /usr/local/lib/kallipai/*/bin
for u in kallipai-archeion kallipai-lesche kallipai-files kallipai-instances; do
  getent passwd "$u" > /dev/null
  getent group "$u" > /dev/null
done
getent group kallipai-polis > /dev/null
getent group kallipai-daemon > /dev/null
id -nG kallipai-instances | tr ' ' '\n' | grep -qx kallipai-daemon
for u in kallipai-daemon kallipai-archeion kallipai-lesche kallipai-files kallipai-instances; do
  test -f "/etc/systemd/system/$u.service"
done
grep -q 'KALLIPAI_BIN_DIR=/usr/local/lib/kallipai/' /etc/systemd/system/kallipai-daemon.service
# rerun stays idempotent
bash /mnt/install.sh --with-systemd --tarball "/mnt/$NAME"
# uninstall removes payload, links, and units
bash /mnt/install.sh --with-systemd --uninstall
test ! -e /usr/local/lib/kallipai
test ! -e /etc/systemd/system/kallipai-daemon.service
"""


def tier(image: str, tarball: Path, work: Path, system: bool) -> None:
    label = f"{image}{' system' if system else ''}"
    script = (SYSTEM_SCRIPT if system else USER_SCRIPT).replace(
        "__TARBALL_NAME__", tarball.name
    )
    script_path = work / ("system-smoke.sh" if system else "user-smoke.sh")
    script_path.write_text(script)
    proc = run(
        [
            "docker",
            "run",
            "--rm",
            "-v",
            f"{tarball.parent}:/mnt:ro",
            "-v",
            f"{script_path}:/smoke.sh:ro",
            image,
            "bash",
            "/smoke.sh",
        ]
    )
    record(
        f"tier {label}",
        proc.returncode == 0,
        (proc.stderr or proc.stdout).strip().splitlines()[-1]
        if proc.returncode != 0
        else "",
    )


def check_units_host(tarball: Path, work: Path) -> None:
    """systemd-analyze verify --root over units rendered on the host with
    the same sed the installer runs (equal form: same templates, same
    payload bin path), against
    stub executables and the host systemd's base target set."""
    import glob

    version = tarball.name.split("kallipai-")[1].split("-linux-x86_64")[0]
    stores = sorted(glob.glob("/nix/store/*-systemd-*/example/systemd/system"))
    if not stores:
        record(
            "systemd-analyze verify (host, --root)",
            False,
            "no systemd package store path with unit templates found",
        )
        return
    bindir = f"/usr/local/lib/kallipai/{version}/bin"
    repo = Path(__file__).resolve().parent.parent
    root = work / "rootfs"
    unitdir = root / "etc/systemd/system"
    unitdir.mkdir(parents=True)
    (root / "usr/lib/systemd/system").mkdir(parents=True)
    # systemd also walks /usr/local/lib/systemd/system; keep it present.
    (root / "usr/local/lib/systemd/system").mkdir(parents=True)
    for src in Path(stores[-1]).glob("*.target"):
        shutil.copy(src, root / "usr/lib/systemd/system/")
    # The database is deployer-provided; a stub keeps the After= resolvable.
    (root / "usr/lib/systemd/system/postgresql.service").write_text(
        "[Unit]\nDescription=postgresql stub (deployer-provided in real installs)\n"
        "[Service]\nType=oneshot\nExecStart=/bin/true\n"
    )
    units = []
    for tpl in sorted((repo / "nix/install/systemd").glob("*.service.in")):
        name = tpl.name.removesuffix(".service.in") + ".service"
        (unitdir / name).write_text(tpl.read_text().replace("@BINDIR@", bindir))
        stub = root / bindir.strip("/") / name.removesuffix(".service")
        stub.parent.mkdir(parents=True, exist_ok=True)
        stub.touch(0o755)
        units.append(f"etc/systemd/system/{name}")
    proc = run(["systemd-analyze", "verify", "--root", str(root), *units])
    record(
        "systemd-analyze verify (host, --root)",
        proc.returncode == 0,
        (proc.stderr or proc.stdout).strip()[-400:] if proc.returncode != 0 else "",
    )


def main() -> int:
    ap = argparse.ArgumentParser(
        description="FHS install smoke (see module docstring for the checks)"
    )
    ap.add_argument(
        "--tarball",
        required=True,
        type=Path,
        help="kallipai-VER-linux-x86_64.tar.gz to stage and install",
    )
    ap.add_argument(
        "--all",
        action="store_true",
        help="run every tier (default: debian:12 only, staged rollout)",
    )
    args = ap.parse_args()

    src = args.tarball.resolve()
    if not src.is_file():
        sys.exit(f"smoke: tarball not found: {src}")
    objdump = subprocess.run(["which", "objdump"], capture_output=True, check=False)
    if objdump.returncode != 0:
        sys.exit("smoke: objdump not found on PATH (binutils)")
    systemd_analyze = subprocess.run(
        ["which", "systemd-analyze"], capture_output=True, check=False
    )
    if systemd_analyze.returncode != 0:
        sys.exit("smoke: systemd-analyze not found on PATH")

    with tempfile.TemporaryDirectory(prefix="fhs-smoke-") as td:
        work = Path(td)
        os.chmod(td, 0o755)  # container user traverses the read-only mount
        # Stage into a writable dir: the store path's sidecar cannot be
        # created next to the artifact, and the container mounts read-only.
        tarball = work / src.name
        shutil.copy(src, tarball)
        import hashlib

        digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
        (work / (src.name + ".sha256")).write_text(f"{digest}  {src.name}\n")
        shutil.copy(
            Path(__file__).resolve().parent.parent / "install.sh", work / "install.sh"
        )
        shutil.copytree(Path(__file__).resolve().parent.parent / "nix", work / "nix")
        # The container's smoke user must read these through the mount
        # regardless of the repo's umask.
        os.chmod(work / "install.sh", 0o755)
        os.chmod(tarball, 0o644)
        os.chmod(work / (src.name + ".sha256"), 0o644)

        check_needed(tarball)
        check_negative_paths(work)
        images = IMAGES if args.all else IMAGES[:1]
        for image in images:
            tier(image, tarball, work, system=False)
            tier(image, tarball, work, system=True)
        check_units_host(tarball, work)

    print()
    for name, passed, _ in results:
        print(f"  [{'PASS' if passed else 'FAIL'}] {name}")
    failed = [r for r in results if not r[1]]
    if failed:
        print(f"\nsmoke: {len(failed)} check(s) failed")
        return 1
    print(f"\nsmoke: all {len(results)} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
