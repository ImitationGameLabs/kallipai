#!/usr/bin/env bash
# install.sh -- install KallipAI from the FHS tarball.
#
# Two modes:
#   user (default)   payload ~/.local/lib/kallipai, links ~/.local/bin
#   --with-systemd   payload /usr/local/lib/kallipai, links /usr/local/bin,
#                    five systemd units in /etc/systemd/system; needs root
#
# Sources:
#   --tarball PATH   install from a local tarball plus its .sha256 sidecar
#   (default)        download from GitHub Releases (--version required)
#
# The versioned payload directory and the data roots stay separate: the
# script never writes ~/.local/share/kallipai, and --uninstall keeps
# /var/lib/kallipai (the systemd state tree) plus the system users and
# groups, printing manual cleanup hints instead.
set -euo pipefail

# Mode constants: each defined exactly once.
USER_LOAD_ROOT="$HOME/.local/lib/kallipai"
USER_BIN_ROOT="$HOME/.local/bin"
SYS_LOAD_ROOT="/usr/local/lib/kallipai"
SYS_BIN_ROOT="/usr/local/bin"
UNIT_DIR="/etc/systemd/system"
SYS_STATE_ROOT="/var/lib/kallipai"      # systemd state; uninstall keeps it
GLIBC_MIN="2.35"
# Order mirrors the unit dependency chain; instances is last (it dials
# the daemon socket and reads the archeion token).
UNITS=(
  kallipai-daemon
  kallipai-archeion
  kallipai-lesche
  kallipai-files
  kallipai-instances
)
RELEASE_URL_BASE="https://github.com/kallipai/kallipai/releases/download"

STEP="init"
trap 'echo "install.sh: error at step \"${STEP}\" (line $LINENO)" >&2' ERR
step() { STEP="$1"; echo "==> $1"; }
die() { echo "install.sh: error: $*" >&2; exit 1; }

# --- environment gates, factored for the smoke suite ---
# scripts/fhs-smoke.py sources this file and calls the two checks
# against forged inputs (no aarch64 host or old glibc needed there).
check_arch() {  # optional arg: machine arch to test (default: this host)
  local arch="${1:-$(uname -m)}"
  [ "$arch" = "x86_64" ] || die "unsupported architecture '$arch': this build is x86_64 only"
}
check_glibc() {  # optional arg: a glibc version to test (default: this host)
  local ver="${1:-$(ldd --version | sed -n '1s/[^0-9]*\([0-9][0-9.]*\).*/\1/p')}"
  [ -n "$ver" ] || die "could not parse a glibc version"
  # Numeric compare on major.minor; report both values on failure.
  # Guard the parse: a musl system's ldd line yields numbers without a
  # minor part; refuse rather than mis-compare.
  case "$ver" in *.*) : ;; *) die "could not parse a glibc version (got '$ver'); musl hosts are unsupported" ;; esac
  local oldest
  oldest="$( { echo "$GLIBC_MIN"; echo "$ver"; } | sort -V | head -1)"
  [ "$oldest" = "$GLIBC_MIN" ] || die "glibc $ver is too old: this build needs >= $GLIBC_MIN"
}

# The smoke suite sources this file to reach the two checks; this stop
# keeps the main flow out of the sourcing shell.
# shellcheck disable=SC2317  # the exit covers direct execution, not sourcing
if [ "${KALLIPAI_INSTALL_SKIP_MAIN:-0}" = 1 ]; then return 0 2>/dev/null || exit 0; fi

usage() {
  cat <<'USAGE'
Usage: install.sh [options]

Install KallipAI from the FHS tarball. Default is a per-user install
under ~/.local (no root, no systemd units).

Options:
  --tarball PATH   Install from a local tarball (with its .sha256
                   sidecar next to it). This is the smoke-test path.
  --version V      Release version to download (default mode only).
  --with-systemd   System-level install: /usr/local payload and links,
                   five systemd units, dedicated system users. Requires
                   root and the repository checkout (for the unit
                   templates under nix/install/systemd/).
  --uninstall      Remove the installed payload, links, and (with
                   --with-systemd) the unit files. Data roots and
                   system users/groups are kept, with printed hints.
  -h, --help       Show this help.

Examples:
  ./install.sh --tarball ./kallipai-1.0.0-linux-x86_64.tar.gz
  sudo ./install.sh --tarball ./kallipai-1.0.0-linux-x86_64.tar.gz --with-systemd
  sudo ./install.sh --with-systemd --uninstall
USAGE
}

ACTION="install"
SYSTEM_MODE=0
TARBALL=""
VERSION=""

while [ $# -gt 0 ]; do
  case "$1" in
    --tarball) [ $# -ge 2 ] || die "--tarball needs a path"; TARBALL="$2"; shift 2 ;;
    --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
    --with-systemd) SYSTEM_MODE=1; shift ;;
    --uninstall) ACTION="uninstall"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
done

# --- shared checks (install and uninstall) ---

if [ "$ACTION" = "uninstall" ]; then
  if [ "$SYSTEM_MODE" = 1 ]; then
    [ "$(id -u)" = 0 ] || die "--with-systemd --uninstall needs root (try: sudo)"
    LOAD_ROOT="$SYS_LOAD_ROOT"
    BIN_ROOT="$SYS_BIN_ROOT"
  else
    LOAD_ROOT="$USER_LOAD_ROOT"
    BIN_ROOT="$USER_BIN_ROOT"
  fi
  step "removing payload tree"
  if [ -d "$LOAD_ROOT" ]; then
    rm -rf "$LOAD_ROOT"
  else
    echo "    nothing to remove at $LOAD_ROOT"
  fi

  step "removing links pointing into the payload"
  if [ -d "$BIN_ROOT" ]; then
    # Whitelist by link target: only links that resolve into this
    # install's payload root are removed; anything else stays.
    for link in "$BIN_ROOT"/*; do
      [ -L "$link" ] || continue
      target="$(readlink "$link")"
      case "$target" in
        "$LOAD_ROOT"/*) rm -f "$link" ;;
        *) echo "    keeping $link (points outside the payload)" ;;
      esac
    done
  fi

  if [ "$SYSTEM_MODE" = 1 ]; then
    step "removing systemd units"
    if [ -d /run/systemd/system ]; then
      for unit in "${UNITS[@]}"; do
        systemctl stop "$unit" 2>/dev/null || true
        systemctl disable "$unit" 2>/dev/null || true
        rm -f "$UNIT_DIR/$unit.service"
      done
      systemctl daemon-reload
    else
      for unit in "${UNITS[@]}"; do rm -f "$UNIT_DIR/$unit.service"; done
      echo "    systemd not running; unit files removed without daemon-reload"
    fi
    echo
    echo "Kept for data safety (remove manually if desired):"
    echo "  state tree:   sudo rm -rf $SYS_STATE_ROOT"
    echo "  users/groups: kallipai-archeion kallipai-lesche kallipai-files"
    echo "                kallipai-instances (users), their primary groups,"
    echo "                kallipai-polis and kallipai-daemon (gate groups)"
  else
    echo
    echo "Kept for data safety: $HOME/.local/share/kallipai is untouched."
  fi
  step "uninstall complete"
  exit 0
fi

# --- install path ---

step "checking architecture"
check_arch
step "checking glibc"
check_glibc

if [ "$SYSTEM_MODE" = 1 ]; then
  LOAD_ROOT="$SYS_LOAD_ROOT"
  BIN_ROOT="$SYS_BIN_ROOT"
  [ "$(id -u)" = 0 ] || die "--with-systemd needs root (try: sudo)"
else
  LOAD_ROOT="$USER_LOAD_ROOT"
  BIN_ROOT="$USER_BIN_ROOT"
fi

if [ -n "$TARBALL" ]; then
  step "verifying tarball checksum"
  TARBALL="$(readlink -f "$TARBALL")"
  [ -f "$TARBALL" ] || die "tarball not found: $TARBALL"
  SHA_FILE="$TARBALL.sha256"
  [ -f "$SHA_FILE" ] || die "checksum sidecar not found: $SHA_FILE"
  (cd "$(dirname "$TARBALL")" && sha256sum -c --status "$(basename "$SHA_FILE")") \
    || die "checksum mismatch for $(basename "$TARBALL")"
else
  step "downloading release"
  [ -n "$VERSION" ] || die "download mode needs --version (or use --tarball for a local file)"
  # Keep the release file name: the .sha256 sidecar references it,
  # and the version parsing below reads it back from the name.
  DL_DIR="$(mktemp -d)"
  TARBALL="$DL_DIR/kallipai-$VERSION-linux-x86_64.tar.gz"
  curl -fL --retry 3 -o "$TARBALL" "$RELEASE_URL_BASE/$VERSION/$(basename "$TARBALL")"
  curl -fL --retry 3 -o "$TARBALL.sha256" "$RELEASE_URL_BASE/$VERSION/$(basename "$TARBALL").sha256"
  (cd "$DL_DIR" && sha256sum -c --status "$(basename "$TARBALL").sha256") \
    || die "checksum mismatch for the downloaded tarball"
fi

step "parsing version from tarball name"
TARBALL_BASE="$(basename "$TARBALL")"
VERSION_DIR="$(printf '%s\n' "$TARBALL_BASE" | sed -n 's/^kallipai-\(.*\)-linux-x86_64\.tar\.gz$/\1/p')"
[ -n "$VERSION_DIR" ] || die "cannot parse a version from '$TARBALL_BASE' (expected kallipai-<version>-linux-x86_64.tar.gz)"

step "unpacking to $LOAD_ROOT/$VERSION_DIR"
mkdir -p "$LOAD_ROOT/$VERSION_DIR"
tar -xzf "$TARBALL" -C "$LOAD_ROOT/$VERSION_DIR"

step "linking binaries into $BIN_ROOT"
mkdir -p "$BIN_ROOT"
for entry in "$LOAD_ROOT/$VERSION_DIR/bin"/*; do
  name="$(basename "$entry")"
  ln -sfn "$LOAD_ROOT/$VERSION_DIR/bin/$name" "$BIN_ROOT/$name"
done

if [ "$SYSTEM_MODE" = 1 ]; then
  step "ensuring system groups and users"
  ensure_group() {
    getent group "$1" >/dev/null || groupadd --system "$1"
  }
  ensure_user() { # name primary-group extra-groups
    if ! getent passwd "$1" >/dev/null; then
      NOLOGIN="$(command -v nologin || echo /usr/sbin/nologin)"
      useradd --system --gid "$2" --shell "$NOLOGIN" "$1"
    fi
    # Mirror the NixOS module's supplementary groups (idempotent);
    # an empty extra list means the account carries none.
    if [ -n "$3" ]; then usermod -a -G "$3" "$1"; fi
  }
  # The platform gate group and the daemon socket gate group.
  ensure_group kallipai-polis
  ensure_group kallipai-daemon
  # Per-service primary groups, then the users (NixOS module mirror:
  # lesche/files ride kallipai-polis; instances is the one polis user
  # that also joins the daemon socket gate. The archeion account
  # deliberately carries no supplementary group: its unit already runs
  # with the platform gate group as its primary.
  for svc in archeion lesche files instances; do
    ensure_group "kallipai-$svc"
  done
  ensure_user kallipai-archeion kallipai-archeion ""
  ensure_user kallipai-lesche   kallipai-lesche   kallipai-polis
  ensure_user kallipai-files    kallipai-files    kallipai-polis
  ensure_user kallipai-instances kallipai-instances "kallipai-polis,kallipai-daemon"

  step "rendering systemd units"
  TEMPLATE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/nix/install/systemd"
  [ -d "$TEMPLATE_DIR" ] || die "unit templates not found at $TEMPLATE_DIR: \
--with-systemd needs the repository checkout (user-mode installs carry no such dependency)"
  BINDIR="$LOAD_ROOT/$VERSION_DIR/bin"
  for unit in "${UNITS[@]}"; do
    sed "s|@BINDIR@|$BINDIR|g" "$TEMPLATE_DIR/$unit.service.in" > "$UNIT_DIR/$unit.service"
  done

  if [ -d /run/systemd/system ]; then
    step "enabling units"
    systemctl daemon-reload
    for unit in "${UNITS[@]}"; do
      systemctl enable "$unit"
    done

    step "restarting units that are already active"
    # Upgrade semantics: restart only what is running (try-restart);
    # nothing inactive gets started by an install. KillMode=process on
    # the daemon keeps running tagmata alive across the restart; they
    # adopt the new binary at their next deliberate kallipctl stop/start.
    for unit in "${UNITS[@]}"; do
      if systemctl is-active --quiet "$unit"; then
        systemctl try-restart "$unit"
      fi
    done
  else
    step "systemd not detected"
    echo "    /run/systemd/system is absent: units were rendered to $UNIT_DIR"
    echo "    but not enabled. Run the binaries directly or manage them"
    echo "    with your init system."
  fi

  step "checking for processes still running from an older version"
  # Loud detection only -- nothing is killed. A bare process from an old
  # version may be an interactive session or a live tagma; list it and
  # let the operator decide.
  STALE=0
  for pid in $(pgrep -f "^$LOAD_ROOT/" || true); do
    exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
    case "$exe" in
      "$LOAD_ROOT/$VERSION_DIR/"*) : ;;              # current version: fine
      "$LOAD_ROOT"/*)
        STALE=1
        echo "    pid $pid still runs from an old version: $exe"
        ;;
    esac
  done
  if [ "$STALE" = 1 ]; then
    echo "    stop them with kallipctl (or wait for exit) and re-run this"
    echo "    script; old versions still in use are not cleaned up."
  fi
else
  step "checking PATH"
  case ":$PATH:" in
    *":$BIN_ROOT:"*) : ;;
    *)
      echo "    $BIN_ROOT is not on your PATH. Add it, e.g.:"
      case "${SHELL:-}" in
        */fish) echo "      fish_add_path $BIN_ROOT" ;;
        *) echo "      export PATH=\"$BIN_ROOT:\$PATH\"   # in ~/.bashrc or ~/.zshrc" ;;
      esac
      ;;
  esac
fi

step "cleaning up old versions"
# Keep the version just installed (and any version a process still runs
# from -- those were reported above); remove the rest.
if [ -d "$LOAD_ROOT" ]; then
  for dir in "$LOAD_ROOT"/*; do
    name="$(basename "$dir")"
    [ "$name" = "$VERSION_DIR" ] && continue
    IN_USE=0
    for pid in $(pgrep -f "^$LOAD_ROOT/" || true); do
      exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
      case "$exe" in
        "$LOAD_ROOT/$name/"*) IN_USE=1; echo "    keeping $name (pid $pid runs from it)" ;;
      esac
    done
    if [ "$IN_USE" = 0 ]; then
      rm -rf "$dir"
      echo "    removed $name"
    fi
  done
fi

step "install complete"
echo "  payload: $LOAD_ROOT/$VERSION_DIR"
echo "  links:   $BIN_ROOT"
if [ "$SYSTEM_MODE" = 1 ]; then
  echo "  units:   $UNIT_DIR (${UNITS[*]})"
  echo
  echo "Next steps (provided by the deployer, mirroring the NixOS module):"
  echo "  - PostgreSQL: one database and role per service (kallipai-archeion,"
  echo "    kallipai-lesche, kallipai-files) with peer auth on the unix socket"
  echo "  - Optional settings: systemctl edit <unit>  (drop-in environment)"
fi
