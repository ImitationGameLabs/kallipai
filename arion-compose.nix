# Arion composition for local dev + the integration-test suite.
#
# Dev is the default (`arion up`); the integration suite is the single opt-in
# mode (`KALLIP_ARION_MODE=test arion up`). The two are structurally different
# (dev = full local stack; test = one-shot runner), so each is a named,
# self-contained composition below, selected by `if isTest`.
#
# Production is NOT here: prod-tagma / prod-agora are standalone compositions
# under nix/prod-composes/, invoked with `arion -f nix/prod-composes/<name>.nix`.
#
# Both modes consume the flake's pre-built outputs directly -- arion does no
# Rust/crane building. Dev runs `packages.default` via useHostStore so the host
# /nix/store is shared and rebuilds are picked up without an in-compose bake;
# the daemon + herald are gated behind the `tagma` profile (the herald can't
# enroll until a user signs up and mints a code). See docs/development.md for
# the bring-up commands and flow.
{ pkgs, lib, ... }:
let
  mode = builtins.getEnv "KALLIP_ARION_MODE";
  # Dev is the default and needs no env var (unset ""); `test` is the single
  # opt-in mode. Any other value is a HARD error, not a silent dev fallback -- a
  # silent fallback would run the dev stack (useHostStore, published ports,
  # POSTGRES_PASSWORD=kallip, localhost WebAuthn) where the operator meant prod.
  # prod-tagma / prod-agora used to be modes here; they are now standalone
  # compositions, so a stale KALLIP_ARION_MODE=prod-* must point the operator at
  # the right file rather than fail cryptically.
  isTest =
    if
      builtins.elem mode [
        ""
        "test"
      ]
    then
      mode == "test"
    else
      abort "arion: KALLIP_ARION_MODE='${mode}' is not supported by arion-compose.nix. Dev is the default (just unset KALLIP_ARION_MODE); the only opt-in mode is 'test'. prod-tagma and prod-agora are now standalone compositions -- run 'arion -f nix/prod-composes/tagma.nix up -d' or 'arion -f nix/prod-composes/agora.nix up -d'.";

  # Read a bind-override env var, returning "<host>:<target>" or null when
  # unset. The value must be empty or an absolute, colon-free path other than
  # "/", otherwise:
  #   - a bare name is parsed by compose as a named-volume ref (-> "undefined volume");
  #   - a colon lets docker mis-parse the src:dst[:mode] string and silently mount
  #     to the wrong target (-> data loss on arion down);
  #   - "/" would bind the host root into the container.
  # Throws at eval in dev; in test mode the bindings are lazily ignored (never
  # referenced), so a bad value is silent there. (prod-tagma has its own copy
  # in nix/prod-composes/tagma.nix.)
  bindOverride =
    name: target:
    let
      v = builtins.getEnv name;
    in
    if v == "" then
      null
    else if v == "/" || !(lib.hasPrefix "/" v) || lib.hasInfix ":" v then
      throw "arion: ${name} must be an absolute, colon-free host path other than '/' (got '${v}')"
    else
      "${v}:${target}";

  # Bind overrides: unset -> docker named volume; set to a host path ->
  # bind-mount:
  #   - data: keep daemon state on a known disk;
  #   - workspace: make the agent's files host-visible;
  #   - skills: curate shared skills on the host.
  dataBind = bindOverride "KALLIP_ARION_DATA_PATH" "/var/lib/kallip";
  workspaceBind = bindOverride "KALLIP_ARION_WORKSPACE_PATH" "/workspace";
  # Skills overlay the data volume's skills/ subdir (no named volume of their
  # own). NOTE: KALLIP_SKILLS_ROOT (if set via .env) short-circuits skill_dir()
  # and bypasses this bind, so leave it unset when using skillsBind.
  skillsBind = bindOverride "KALLIP_ARION_SKILLS_PATH" "/var/lib/kallip/skills";

  dataVolume = if dataBind != null then dataBind else "data:/var/lib/kallip";
  workspaceVolume = if workspaceBind != null then workspaceBind else "workspace:/workspace";

  # Load via git+file URL (not a bare path) so getFlake applies fetchGit's VCS
  # filtering and the resolved packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ./.}";
  workspace = flake.packages.x86_64-linux.default;
  integrationTests = flake.packages.x86_64-linux.kallip-integration-tests;

  # Shared toolset + certs + aifed + PATH. Reused by the tagma docker image
  # (nix/packages/docker-images/tagma.nix) and here in the dev compose, so the
  # two cannot drift.
  shared = import ./nix/packages/container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    certLinks
    aifed
    binPath
    ;

  # The runtime image tail common to dev and test (the mode-specific package --
  # `workspace` or `integrationTests` -- is prepended by each composition).
  # Mirrors the toolset/CA layer of nix/packages/docker-images/tagma.nix.
  runtimeContents = [
    toolEnv
    pkgs.cacert
    certLinks
    aifed
  ];

  # The daemon only, looping over every prebuilt [[test]] binary. Exits with
  # the overall verdict.
  testComposition = {
    config = {
      project.name = "kallipai-test";

      services.daemon = {
        # The daemon's landlock/seccomp shell sandbox needs these privileges
        # (see docs/reference/container.md). No typed option for security_opt;
        # out.service is the documented escape hatch (attrsOf, merges with the
        # computed spec).
        service.capabilities.SYS_ADMIN = true;
        out.service.security_opt = [ "seccomp=unconfined" ];
        # Adds root-level /bin/sh and /usr/bin/env symlinks. The daemon and
        # agent shells don't need them (bash is resolved via PATH/toolEnv); this
        # is a convenience for `arion exec` and the iterate script below relies
        # on /bin/sh.
        image.enableRecommendedContents = true;
        image.contents = [ integrationTests ] ++ runtimeContents;
        service.useHostStore = true;
        # Run every pre-built [[test]] binary:
        #   - `--nocapture` surfaces scenario eprintln! diagnostics in `arion logs`;
        #   - each libtest harness exits non-zero on failure, so `arion ps -a`
        #     reports the overall verdict;
        #   - `set -e` fail-fasts across binaries (order is alphabetical: exec,
        #     then sandbox -- an early failure would mask later ones);
        #   - `[ -e ]` guards an empty glob; `found` refuses to silently pass
        #     when no test binary is present.
        # restart = "no" so the exited container isn't restarted.
        # Caveat: --nocapture assumes each binary is a libtest harness; a future
        # [[test]] with `harness = false` would reject the flag and fail fast.
        service.command = [
          "/bin/sh"
          "-c"
          ''
            set -e
            found=0
            for t in /integration-tests/*; do
              [ -e "$t" ] || continue
              found=1
              echo "=== integration test: $t ==="
              "$t" --nocapture
            done
            [ "$found" = 1 ] || { echo "no integration tests found"; exit 1; }
          ''
        ];
        service.restart = "no";
        # /testdata holds the sandbox scenarios' home/data/workspace scratch
        # dirs. It MUST be outside libsandbox's baseline-writable set (/tmp,
        # /var/tmp, $TMPDIR) so write-denial assertions stay honest; a dedicated
        # tmpfs is the simplest such path. Same escape hatch as security_opt.
        out.service.tmpfs = [ "/testdata:rw,size=64m" ];
        service.environment = {
          PATH = "${integrationTests}/bin:${binPath}";
          # Explicit agent-bin dir for resolve_bin -- current_exe() resolves
          # the buildEnv symlink into a sub-store path, not the shared bin/.
          KALLIP_BIN_DIR = "${integrationTests}/bin";
          KALLIP_TESTDATA_DIR = "/testdata";
          HOME = "/var/lib/kallip";
          RUST_LOG = "info";
        };
      };
    };
  };

  # The full local stack: agora + postgres + daemon + herald. The daemon +
  # herald are gated behind the `tagma` profile (see header) so a plain
  # `arion up` brings up only the agora side. Daemon data, the workspace, and
  # shared skills default to docker volumes; each can be bind-overridden via
  # the KALLIP_ARION_*_PATH vars above.
  devComposition = {
    config = {
      project.name = "kallipai-dev";

      # Named volumes must be declared at the compose top level (compose rejects
      # a reference to an undeclared named volume):
      #   - pgdata + herald-state: always named volumes;
      #   - data + workspace: declared only when their override env var is unset
      #     (otherwise that mount is a bind mount and no named volume is referenced).
      docker-compose.volumes = {
        pgdata = { };
        herald-state = { };
      }
      // lib.optionalAttrs (dataBind == null) { data = { }; }
      // lib.optionalAttrs (workspaceBind == null) { workspace = { }; };

      # Dev daemon. The landlock/seccomp shell sandbox needs SYS_ADMIN +
      # seccomp=unconfined (out.service is the escape hatch for security_opt).
      services.daemon = {
        service.capabilities.SYS_ADMIN = true;
        out.service.security_opt = [ "seccomp=unconfined" ];
        out.service.profiles = [ "tagma" ];
        service.ports = [ "3000:3000" ];
        service.volumes = [
          dataVolume
          workspaceVolume
        ]
        # skills has no named volume of its own: unset -> skills live inside
        # the `data` volume's skills/ subdir; set -> a bind overlays it.
        ++ lib.optional (skillsBind != null) skillsBind;
        service.env_file = [ ".env" ];
        image.enableRecommendedContents = true;
        image.contents = [ workspace ] ++ runtimeContents;
        service.useHostStore = true;
        service.command = [ "${workspace}/bin/kallip-daemon" ];
        service.environment = {
          PATH = binPath;
          HOME = "/var/lib/kallip";
          KALLIP_DATA_DIR = "/var/lib/kallip";
          # Default workspace for clients (e.g. the TUI) that create an agent
          # without an explicit workspace_root: AgentConfig::load otherwise
          # falls back to the daemon's cwd, which is "/" in the container and
          # overlaps the data dir -> 409. Pin the mounted workspace volume,
          # which is disjoint from /var/lib/kallip.
          KALLIP_WORKSPACE_ROOT = "/workspace";
          KALLIP_DAEMON_ADDR = "0.0.0.0:3000";
          RUST_LOG = "info";
        };
      };

      # Postgres: the official image, run as its own container for production
      # parity and isolation. Agora retries it with a capped backoff at boot, so
      # `depends_on` (start-order only) is enough -- no healthcheck. (prod-agora's
      # postgres lives in nix/prod-composes/agora.nix and reads creds from .env.)
      services.postgres = {
        service.image = "postgres:17.5";
        service.volumes = [ "pgdata:/var/lib/postgresql/data" ];
        # dev-only hardcoded credentials. (environment wins over env_file, so
        # these MUST stay dev-only -- prod reads them from .env.)
        service.environment = {
          POSTGRES_USER = "kallip";
          POSTGRES_PASSWORD = "kallip";
          POSTGRES_DB = "kallip";
        };
      };

      # Agora: run from the workspace via the host store; publish 7100 so the
      # host browser (kallip-web at :5173) reaches it; dev WebAuthn / CORS /
      # cookie values are localhost-friendly. (prod-agora is its own composition:
      # nix/prod-composes/agora.nix, behind a TLS reverse proxy, no published
      # port.)
      services.agora = {
        service.depends_on = [ "postgres" ];
        service.useHostStore = true;
        service.command = [ "${workspace}/bin/kallip-agora" ];
        service.ports = [ "7100:7100" ];
        # Optional stable admin token (else generated per boot, printed to
        # `arion logs agora`).
        service.env_file = [ ".env" ];
        service.environment = {
          KALLIP_AGORA_ADDR = "0.0.0.0:7100";
          KALLIP_AGORA_DATABASE_URL = "postgres://kallip:kallip@postgres:5432/kallip";
          KALLIP_AGORA_WEBAUTHN_RP_ID = "localhost";
          KALLIP_AGORA_WEBAUTHN_RP_ORIGIN = "http://localhost:5173";
          KALLIP_AGORA_WEBAUTHN_RP_NAME = "kallip";
          KALLIP_AGORA_WEBAUTHN_ALLOW_ANY_PORT = "true";
          KALLIP_AGORA_COOKIE_SECURE = "false";
          KALLIP_AGORA_CORS_ORIGINS = "http://localhost:5173";
          RUST_LOG = "info";
          # TRUSTED_PROXIES left at the loopback default; the agora's boot guard
          # logs a cosmetic warning about the 0.0.0.0 bind here -- expected in
          # dev (no reverse proxy in front), harmless.
        };
      };

      # Herald: persists its device key + tagma token in `herald-state`, so it
      # re-enrolls only on the very first boot (using
      # KALLIP_HERALD_ENROLLMENT_CODE from .env, minted via the agora dashboard).
      # Its first-boot enroll() is NOT retried in code, so `restart:
      # unless-stopped` lets it come back once the code is supplied / the agora
      # is up (a bad code crashloops -- documented). Gated behind the `tagma`
      # profile alongside the daemon. (prod-tagma's herald lives in
      # nix/prod-composes/tagma.nix.)
      services.herald = {
        service.command = [ "${workspace}/bin/kallip-herald" ];
        service.volumes = [ "herald-state:/var/lib/kallip/herald" ];
        service.restart = "unless-stopped";
        # KALLIP_AUTH_TOKEN (herald -> daemon operator token) +
        # KALLIP_HERALD_ENROLLMENT_CODE (first run only).
        service.env_file = [ ".env" ];
        out.service.profiles = [ "tagma" ];
        service.useHostStore = true;
        service.depends_on = [
          "agora"
          "daemon"
        ];
        service.environment = {
          KALLIP_HERALD_STATE_DIR = "/var/lib/kallip/herald";
          KALLIP_HERALD_AGORA_URL = "http://agora:7100";
          KALLIP_DAEMON_URL = "http://daemon:3000";
          RUST_LOG = "info";
        };
      };
    };
  };
in
if isTest then testComposition else devComposition
