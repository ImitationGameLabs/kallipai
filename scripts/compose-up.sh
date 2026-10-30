#!/usr/bin/env bash
# Bring the platform stack up with docker compose.
#
#   scripts/compose-up.sh [dev|prod] [-- EXTRA...]
#   scripts/compose-up.sh check-env <dev|prod> [-- VERB]
#
# What it does, in order: check for the docker CLI and the compose plugin,
# render the two files the way a start would (compose reports a name it cannot
# fill, so a broken environment stops before anything is built), build and load
# the images this checkout needs (a teardown or an inspection skips that step),
# make sure the shared platform network exists, and hand the two files to
# `docker compose up -d`. The requirements are the docker CLI with the compose
# plugin (2.20 or newer), the nix CLI for the images, and python3, which reads
# the platform network's name out of the rendered config.
# EXTRA replaces the default `up -d`, so `-- down` or `-- config` work the same
# way.
#
# The compose files never pull an application image: the names they interpolate
# are what this script loaded into the local docker store moments earlier.
#
# `check-env` runs the guards the named verb would run and the render that verb
# needs, touching neither the daemon nor the images: a start that would fail on
# the environment falls here instead. The render needs the compose plugin, which
# reads the files without asking the daemon anything. With no VERB it checks a
# start; `check-env dev -- down` checks a teardown, which the guards let
# through. A mode left out means dev, as it does for the other verbs.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

env_script="$repo_root/scripts/compose-env.sh"
min_compose="2.20"

usage() {
  echo "usage: $0 [dev|prod] [-- EXTRA...]" >&2
  echo "       $0 check-env <dev|prod> [-- VERB]" >&2
  exit 2
}

# `check-env` is a verb, so it may come before or after the mode:
# `compose-up.sh check-env prod` and `compose-up.sh prod -- check-env` mean the
# same thing.
verb=
mode=${1:-dev}
case "$mode" in
  dev | prod) shift || true ;;
  check-env)
    verb=check-env
    shift || true
    mode=${1:-dev}
    case "$mode" in
      dev | prod) shift || true ;;
      *) usage ;;
    esac
    ;;
  *) usage ;;
esac
if [ "${1:-}" = "--" ]; then
  shift
fi
# `check-env` after `--` means what it means before the mode: the verb this
# script acts on, not a compose command.
if [ "${1:-}" = check-env ]; then
  verb=check-env
  shift || true
  if [ "${1:-}" = "--" ]; then
    shift
  fi
fi
# The verb a start would receive: the word after `--`. `check-env` judges that
# word instead of running it, so what it checks is exactly what the start would
# get, guards and all.
target_verb=${1:-up}
# The verb decides what has to be in place: starting a stack needs the mkcert
# pair, while a teardown or an inspection only needs the variables the compose
# files interpolate.
case "$target_verb" in
  down | ps | logs | config | stop) starts=0 ;;
  *) starts=1 ;;
esac
# `down`, `stop`, `ps` and `logs` read the stack, they do not run it: the images
# only have to be names there, and building what a teardown removes wastes a
# minute. `config` is the exception, it prints the real thing, and `check-env`
# builds nothing by definition.
images=1
case "$target_verb" in
  down | stop | ps | logs) images=0 ;;
esac
# The render resolves the values a start acts on; a verb that only reads the
# stack renders nothing, since compose needs no service env file for it and that
# file is what a half-set-up checkout is most likely to lack.
needs_render=1
case "$target_verb" in
  down | stop | ps | logs) needs_render=0 ;;
esac

# --- the files and the environment they render with ---------------------------
# compose interpolates from the `--env-file` files this script hands it, in
# the order given, the last one winning, while a variable already exported
# wins over all of them; once `--env-file` is passed, compose no longer
# reads a `.env` from the project directory by itself. So the dev fallbacks
# travel in a temp file handed over first, the stack's env file, compose/.env
# beside the compose files, follows when it exists, and the values this script
# computes -- the stack directory, the image tag, the dev image -- are
# exported and cannot be shadowed by either. An env file full of quotes,
# comments and inline comments needs no reader here: compose resolves it. The
# status script hands over the same flags, so the two have to move together.
env_files=()
defaults_file=""
if [ "$mode" = dev ]; then
  defaults_file=$(mktemp)
  trap 'rm -f "$defaults_file"' EXIT
  bash "$env_script" --defaults dev >"$defaults_file"
  env_files+=(--env-file "$defaults_file")
fi
if [ -f compose/.env ]; then
  env_files+=(--env-file compose/.env)
fi

# The values this script computes stand in with placeholders while compose
# renders the files; the real ones replace them below.
export KALLIPAI_IMAGE_TAG=${KALLIPAI_IMAGE_TAG:-unloaded}
export KALLIPAI_STACK_DIR=${KALLIPAI_STACK_DIR:-/unused}
if [ "$mode" = dev ]; then
  export KALLIPAI_DEV_IMAGE=${KALLIPAI_DEV_IMAGE:-localhost/kallipai-dev:unloaded}
fi

compose() {
  docker compose "${env_files[@]}" -f compose/compose.yaml -f "compose/compose.$mode.yaml" "$@"
}

# Both tools are needed before the first call, so a host without them hears it
# here rather than from the shell. The client and its version need no daemon,
# which is what `check-env` promises to stay clear of.
command -v docker >/dev/null || {
  echo "compose-up: no docker CLI on PATH" >&2
  exit 1
}
compose_version=$(docker compose version --short 2>/dev/null || true)
if [ -z "$compose_version" ]; then
  echo "compose-up: 'docker compose' is missing; install the compose plugin" >&2
  exit 1
fi
if [ "$(printf '%s\n%s\n' "$min_compose" "$compose_version" | sort -V | head -1)" != "$min_compose" ]; then
  echo "compose-up: compose $compose_version is older than $min_compose" >&2
  exit 1
fi

# What compose resolves is what the guards below judge, and also the check a
# start would get: a name that neither the stack's env file, the dev fallbacks
# nor this shell provides is reported by compose, by name, before a single
# image is built. A verb that only reads the stack resolves nothing, and
# every guard that needs a value belongs to a start.
resolved=""
if [ "$needs_render" = 1 ]; then
  compose config --quiet || {
    if [ ! -f compose/.env ]; then
      if [ "$mode" = dev ]; then
        echo "compose-up: this checkout has no compose/.env; cp compose/.env.example compose/.env for a dev start (see docs/en/development/setup.md)" >&2
      else
        echo "compose-up: this deployment has no compose/.env; scripts/compose-env.sh --example prod prints a starting point (see docs/en/deployment/container.md)" >&2
      fi
    fi
    exit 1
  }
  resolved=$(compose config --environment)
fi

# The value of NAME in what compose resolved, or nothing.
env_value() {
  local line
  while IFS= read -r line; do
    case "$line" in
      "$1="*) printf '%s\n' "${line#*=}"; return 0 ;;
    esac
  done <<<"$resolved"
  return 0
}

# The resolved values this script acts on itself.
domain=$(env_value KALLIPAI_DOMAIN)
service_env_file=$(env_value KALLIPAI_ENV_FILE)
image_prefix=$(env_value KALLIPAI_IMAGE_PREFIX)

# The dev edge terminates TLS with the mkcert pair mounted from the checkout.
# Without it docker creates an empty directory at the mount target and caddy
# restarts in a loop while `up -d` still exits 0 -- so a start is refused
# before anything is built. A teardown or an inspection needs neither, and a
# start on the plain entry mounts no certificate at all.
# The entry a start would run comes from what compose resolved, so a value in
# the stack's env file decides exactly as one in this shell does.
edge_entry=$(env_value KALLIPAI_EDGE_ENTRY)
edge_entry=${edge_entry:-/etc/caddy/Caddyfile.vite}
plain_entry=/etc/caddy/Caddyfile.plain
if [ "$mode" = dev ] && [ "$starts" = 1 ] && [ "$edge_entry" != "$plain_entry" ]; then
  for file in cert.pem key.pem; do
    if [ ! -f "$repo_root/compose/dev/.certs/$file" ]; then
      echo "compose-up: compose/dev/.certs/$file is missing (mkcert; see docs/en/development/setup.md)" >&2
      exit 1
    fi
  done
fi

# The plain entry serves http: neither of its site blocks carries a `tls` line,
# so an address that names another scheme sends caddy after a certificate of its
# own -- its internal CA for localhost or a private name, ACME for a public one --
# and that port then answers over TLS with a certificate no browser trusts, while
# `up -d` still exits 0 and the mkcert pair above was never mounted. Both values
# have to be http://: an address without a scheme is an https site to caddy
# whatever its port, with the same result. A missing value is refused too, since
# compose would stop on it one step later. The origin compose derives is https on
# 443, so a plain edge has to name it as well: CORS, the session cookie domain
# and the WebAuthn relying party compare it verbatim.
if [ "$mode" = dev ] && [ "$starts" = 1 ] && [ "$edge_entry" = "$plain_entry" ]; then
  for name in KALLIPAI_EDGE_SITE KALLIPAI_API_BASE; do
    value=$(env_value "$name")
    case "$value" in
      "")
        echo "compose-up: the plain entry serves http, but $name is unset: set it to an http:// address (see docs/en/development/setup.md)" >&2
        exit 1
        ;;
      http://*) ;;
      *://*)
        echo "compose-up: the plain entry serves http, but $name is '$value': set it to an http:// address (see docs/en/development/setup.md)" >&2
        exit 1
        ;;
      *)
        echo "compose-up: the plain entry serves http, but $name is '$value': set it to an http:// address, a bare host:port is https to caddy (see docs/en/development/setup.md)" >&2
        exit 1
        ;;
    esac
  done
  if [ -z "$(env_value KALLIPAI_APP_ORIGIN)" ]; then
    echo "compose-up: the plain entry serves http, so name the origin the browser types: KALLIPAI_APP_ORIGIN=$(env_value KALLIPAI_EDGE_SITE)" >&2
    exit 1
  fi
fi

# The same mismatch the other way round: the vite entry's site blocks carry a
# `tls` line, and caddy will not load an `http://` address beside it ("server
# listening on [:8080] is HTTP, but attempts to configure TLS connection
# policies"), so the container exits and restarts while `up -d` still exits 0.
# An `https://` address is a different matter and stays allowed: the `tls` line
# then serves that port over TLS, so `https://app.localhost:8080` answers and
# only the port is unusual.
if [ "$mode" = dev ] && [ "$starts" = 1 ] && [ "$edge_entry" != "$plain_entry" ]; then
  for name in KALLIPAI_EDGE_SITE KALLIPAI_API_BASE; do
    value=$(env_value "$name")
    case "$value" in
      http://*)
        echo "compose-up: the $edge_entry entry serves https and caddy will not load '$value' beside its tls line: write the site address as host:port, the entry decides the scheme" >&2
        exit 1
        ;;
    esac
  done
fi

# The https edge's port is KALLIPAI_EDGE_PORT, 443 unless it says otherwise: the
# API base and the HMR client port follow it, and the site address is where the
# edge binds it. So an address naming another port, or naming no port while
# KALLIPAI_EDGE_PORT sits off 443, is refused instead of picking one of the two;
# an address left unset keeps compose's own default, which names the port.
# The site address decides the host: a wildcard answers app.<its own domain>,
# a literal address answers itself, and no address leaves app.<domain>. The
# origin compose derives is https://app.<domain>, the shape that leaves out
# 443, so where the host differs or the edge answers off 443 the origin is
# asked for, and the line offers the address the edge does answer. A value
# that is given is used as written and never compared: what the browser
# types depends on whether a TLS proxy stands in front of the edge, and
# nothing at this point can see one. A host is compared byte for byte and
# offered the way the address spells it, so one whose letters differ only in
# case reads as another host; a browser lowercases the host it sends.
# The plain entry asks for its origin above, so this guard is about the https edge
# alone.
if [ "$mode" = dev ] && [ "$starts" = 1 ] && [ "$edge_entry" != "$plain_entry" ]; then
  edge_site=$(env_value KALLIPAI_EDGE_SITE)
  edge_site=${edge_site#*://}
  edge_port=$(env_value KALLIPAI_EDGE_PORT)
  edge_port=${edge_port:-443}
  edge_name="app.$domain"
  edge_site_port=
  case "$edge_site" in
    "") ;;
    *:*)
      edge_site_host=${edge_site%%:*}
      edge_site_port=${edge_site#"$edge_site_host":}
      # An address with no host leaves the derived app.<domain>.
      if [ -n "$edge_site_host" ]; then edge_name=$edge_site_host; fi
      ;;
    *) edge_name=$edge_site ;;
  esac
  case "$edge_site_port" in
    "")
      if [ -n "$edge_site" ] && [ "$edge_port" != 443 ]; then
        echo "compose-up: the site address names no port while KALLIPAI_EDGE_PORT is $edge_port, and the edge binds the address as written; write $edge_site:$edge_port there, or drop KALLIPAI_EDGE_PORT" >&2
        exit 1
      fi
      ;;
    "$edge_port") ;;
    *)
      echo "compose-up: the site address names port $edge_site_port while KALLIPAI_EDGE_PORT is $edge_port; the API base and the HMR port follow KALLIPAI_EDGE_PORT, so set KALLIPAI_EDGE_PORT=$edge_site_port or drop the port from the site address" >&2
      exit 1
      ;;
  esac
  case "$edge_name" in
    \**)
      edge_name=${edge_name#\*}
      # A bare wildcard answers every host, so the derived app.<domain> is still
      # the address a browser reaches it by.
      if [ -n "$edge_name" ]; then
        edge_name="app$edge_name"
      else
        edge_name="app.$domain"
      fi
      ;;
  esac
  if { [ "$edge_port" != 443 ] || [ "$edge_name" != "app.$domain" ]; } &&
    [ -z "$(env_value KALLIPAI_APP_ORIGIN)" ]; then
    served=https://$edge_name
    [ "$edge_port" = 443 ] || served=$served:$edge_port
    echo "compose-up: the edge answers https on $edge_name port $edge_port, so name the origin the browser types: KALLIPAI_APP_ORIGIN=$served" >&2
    exit 1
  fi
fi

if [ "$verb" = check-env ]; then
  if [ "$needs_render" = 1 ]; then
    echo "compose-up: the environment guards and the compose render for a $mode $target_verb pass"
  else
    echo "compose-up: the environment guards for a $mode $target_verb pass"
  fi
  exit 0
fi

# --- daemon ------------------------------------------------------------------
# The CLI and its version are checked beside the render above, which needs no
# daemon; everything from here on talks to one.
if ! docker version --format '{{.Client.Version}}' >/dev/null 2>&1; then
  echo "compose-up: no docker daemon reachable" >&2
  exit 1
fi

# --- images ------------------------------------------------------------------
# `docker load` prints the reference it created; that reference, not a guess
# at the tag, is what the compose files get.
# Read every line docker load prints and keep the last reference: an early
# exit would close the pipe on a producer that is still writing, which under
# `set -o pipefail` fails the whole substitution.
load_image() {
  local attr=$1 built ref
  built=$(nix build ".#$attr" --no-link --print-out-paths)
  # buildImage writes a tarball; streamLayeredImage writes an executable that
  # streams one on stdout. Feeding the script to `docker load -i` reads a tar
  # header that is not there, so the two forms are told apart by the bit.
  local -a load=(docker load -i "$built")
  if [ -x "$built" ]; then
    load=(bash -c '"$1" | docker load' _ "$built")
  fi
  ref=$("${load[@]}" | while IFS= read -r line; do
    case "$line" in
      "Loaded image: "*) printf '%s\n' "${line#Loaded image: }" ;;
    esac
  done | tail -n 1)
  if [ -z "$ref" ]; then
    echo "compose-up: docker load reported no reference for $attr" >&2
    exit 1
  fi
  # A registry prefix is what the compose files interpolate; the daemon just
  # loaded the unprefixed name, so add the alias it will be asked for.
  if [ -n "$image_prefix" ]; then
    docker tag "$ref" "${image_prefix}${ref}"
    ref="${image_prefix}${ref}"
  fi
  printf '%s\n' "$ref"
}

# --- placeholders ------------------------------------------------------------
# A stack that starts on the example's values starts and serves nothing: the
# stores keep the credentials they were first initialised with, and a
# placeholder that reaches a reachable host is a working credential for
# whoever reads the file first. Refuse a start on them; a teardown or an
# inspection still runs, so a host can be read and cleaned either way. Both
# files are read the way the services read them: a commented-out line is not a
# value. This runs before anything is built, so the cheapest failure is the
# first one reported.
if [ "$starts" = 1 ] && [ "$mode" = prod ]; then
  if [ "$domain" = "example.com" ]; then
    echo "compose-up: KALLIPAI_DOMAIN is still example.com (scripts/compose-env.sh --example prod)" >&2
    exit 1
  fi
  # The guard and compose have to read the same file. compose resolves a
  # relative env_file against the project directory (compose/), while this
  # shell would resolve it against the cwd, so a relative value would scan
  # nothing while compose read the file it named and a placeholder started
  # unnoticed. Only an absolute path reads alike on both sides.
  case $service_env_file in
    "" | /*) ;;
    *)
      echo "compose-up: KALLIPAI_ENV_FILE must be an absolute path, got $service_env_file" >&2
      echo "compose-up: compose resolves a relative one against compose/, this check against the cwd" >&2
      echo "compose-up: give an absolute path, e.g. $repo_root/compose/.env.prod" >&2
      exit 1
      ;;
  esac
  bad=""
  # The service env file holds most of the deployed values; the stack's env
  # file, beside the compose files, holds the ones compose interpolates
  # itself, and GF_SECURITY_ADMIN_PASSWORD is one of those.
  for file in "$service_env_file" "$repo_root/compose/.env"; do
    [ -f "$file" ] || continue
    lineno=0
    while IFS= read -r line; do
      lineno=$((lineno + 1))
      trimmed=${line#"${line%%[![:space:]]*}"}
      case "$trimmed" in
        "" | "#"*) continue ;;
      esac
      case "${trimmed%%[[:space:]]#*}" in
        *change-me*)
          echo "compose-up: $file:$lineno still carries a change-me placeholder (${trimmed%%=*})" >&2
          bad=1
          ;;
      esac
    done < "$file"
  done
  if [ -n "$bad" ]; then
    exit 1
  fi
fi

# The placeholder exported above is what the render checked; a start replaces it
# with the texts this checkout builds.
if [ "$images" = 1 ]; then
  KALLIPAI_STACK_DIR=$(nix build .#kallipai-stack --no-link --print-out-paths)
  export KALLIPAI_STACK_DIR
fi
tag=""

if [ "$images" = 0 ]; then
  # Nothing runs, so the names only have to interpolate: what compose reads is
  # the project label, not the image.
  tag=${KALLIPAI_IMAGE_TAG:-unloaded}
  if [ "$mode" = dev ]; then
    export KALLIPAI_DEV_IMAGE=${KALLIPAI_DEV_IMAGE:-localhost/kallipai-dev:unloaded}
  fi
elif [ "$mode" = dev ]; then
  export KALLIPAI_DEV_IMAGE
  KALLIPAI_DEV_IMAGE=$(load_image kallipai-dev-image)
  tag=${KALLIPAI_DEV_IMAGE##*:}
  while read -r image; do
    load_image "kallipai-$image-image" >/dev/null
  done < <(bash "$env_script" --images dev)
else
  while read -r image; do
    ref=$(load_image "kallipai-$image-image")
    loaded=${ref##*:}
    # Every image here shares the flake's gitVersion; a tag that differs means
    # the stack would name an image this run did not load.
    if [ -n "$tag" ] && [ "$loaded" != "$tag" ]; then
      echo "compose-up: $image loaded as $loaded while earlier images use $tag" >&2
      exit 1
    fi
    tag=$loaded
  done < <(bash "$env_script" --images prod)
fi

# The tag this run loaded is the one the compose files must name: a value
# from the stack's env file would point at images this run never loaded.
if [ -z "$tag" ]; then
  echo "compose-up: could not read a tag from the loaded images" >&2
  exit 1
fi
export KALLIPAI_IMAGE_TAG="$tag"

# --- network -----------------------------------------------------------------
# compose.yaml declares the platform network as external: it outlives a single
# project, so `compose down` never deletes it and a second checkout can join.
# Its name is whatever compose resolves -- the default in compose.yaml, or the
# KALLIPAI_PLATFORM_NETWORK in the stack's env file -- and
# scripts/compose-status.sh probes inside the same one.
if [ "$starts" = 1 ]; then
  network=$(compose config --format json | bash "$env_script" --network)
  if [ -z "$network" ]; then
    echo "compose-up: compose rendered no external platform network" >&2
    exit 1
  fi
  if ! docker network inspect "$network" >/dev/null 2>&1; then
    docker network create "$network" >/dev/null
  fi
fi

# --- up ----------------------------------------------------------------------
if [ "$#" -eq 0 ]; then
  set -- up -d
fi
# compose still reads the fallback file on this call; the EXIT trap removes it.
compose "$@"
