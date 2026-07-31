# Dev tagma composition (agent host + in-process relay connector), split from
# arion-compose.nix so tagma operations don't drag the agora side along and
# there is no COMPOSE_PROFILES dance.
#
# Invoke from the repo root (so .env resolves):
#   arion -f compose/dev/tagma.nix up -d
#
# Bring-up order: start the agora side first (`arion up -d`), sign up and mint a
# `sk-enroll-...` code into `.env` as KALLIP_TAGMA_RELAY_ENROLLMENT_CODE, then
# start this. On a missing/empty enrollment code the tagma degrades to
# local-only and keeps serving local agents.
#
# Runs on the host network (`network_mode: host`) and reaches the agora/lesche
# at 127.0.0.1:7100 / :7200 -- the host-published ports of arion-compose.nix --
# mirroring how the caddy service reaches them. KALLIP_TAGMA_ADDR binds host
# :3000 directly (no `ports:` mapping; ignored under host net anyway). The
# landlock/seccomp shell sandbox still needs SYS_ADMIN + seccomp=unconfined.
#
# Separate compose project (`kallipai-dev-tagma`) so its containers/volumes are
# distinct from the agora-side `kallipai-dev` project. NOTE: if you previously
# ran tagma via the old `kallipai-dev` profile, its named volumes
# (`kallipai-dev_tagma_data` / `_workspace`) are orphaned by this split -- use
# the KALLIP_ARION_*_PATH bind overrides (or `docker volume` migration) if you
# need to keep that state.
{ pkgs, lib, ... }:
let
  flake = builtins.getFlake "git+file://${toString ../..}";
  workspace = flake.packages.x86_64-linux.default;

  shared = import ../../nix/packages/container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    cacert
    aifed
    binPath
    skillsSeed
    ;

  # Bind-override helpers (mirrors arion-compose.nix): unset -> docker named
  # volume; set to an absolute, colon-free host path -> bind-mount.
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
  dataBind = bindOverride "KALLIP_ARION_DATA_PATH" "/var/lib/kallip";
  workspaceBind = bindOverride "KALLIP_ARION_WORKSPACE_PATH" "/workspace";
  skillsBind = bindOverride "KALLIP_ARION_SKILLS_PATH" "/var/lib/kallip/skills";

  dataVolume = if dataBind != null then dataBind else "tagma_data:/var/lib/kallip";
  workspaceVolume = if workspaceBind != null then workspaceBind else "tagma_workspace:/workspace";
in
{
  config = {
    project.name = "kallipai-dev-tagma";

    docker-compose.volumes =
      { }
      // lib.optionalAttrs (dataBind == null) { tagma_data = { }; }
      // lib.optionalAttrs (workspaceBind == null) { tagma_workspace = { }; };

    services.tagma = {
      service.capabilities.SYS_ADMIN = true;
      out.service.security_opt = [ "seccomp=unconfined" ];
      out.service.tmpfs = [ "/tmp:rw,size=256m" ];
      service.restart = "unless-stopped";
      service.network_mode = "host";
      service.volumes = [
        dataVolume
        workspaceVolume
      ]
      # skills has no named volume of its own: unset -> skills live inside the
      # `tagma_data` volume's skills/ subdir; set -> a bind overlays it.
      ++ lib.optional (skillsBind != null) skillsBind;
      service.env_file = [ ".env" ];
      image.enableRecommendedContents = true;
      image.contents = [
        workspace
        toolEnv
        aifed
      ]
      ++ cacert;
      service.useHostStore = true;
      # Expose the host's nix daemon so the tagma (and the agent's sandboxed
      # shells) can realize flakes / self-install tools via nix. Arion's option
      # atomically sets NIX_REMOTE=daemon and bind-mounts the host
      # /nix/var/nix/daemon-socket dir; the closure itself comes from the host
      # store via useHostStore above.
      service.useHostNixDaemon = true;
      service.command = [ "${workspace}/bin/kallip-tagma" ];
      service.environment = {
        PATH = "${workspace}/bin:${binPath}";
        # The in-container nix client has no /etc/nix/nix.conf (only the store +
        # daemon socket are shared), so it falls back to defaults where
        # flakes/nix-command are off. Enable them client-side; the daemon still
        # owns build/substitution policy.
        NIX_CONFIG = "extra-experimental-features = nix-command flakes";
        HOME = "/var/lib/kallip";
        KALLIP_DATA_DIR = "/var/lib/kallip";
        # The tagma eagerly creates the singleton root agent at startup; its
        # workspace is resolved by AgentConfig::load from KALLIP_WORKSPACE_ROOT.
        # Pin the mounted workspace volume, which is disjoint from
        # /var/lib/kallip (the data dir) -- a CWD fallback would be "/" in the
        # container, overlap the data tree, and fail startup
        # (ensure_workspace_disjoint rejects the overlap).
        KALLIP_WORKSPACE_ROOT = "/workspace";
        KALLIP_TAGMA_ADDR = "0.0.0.0:3000";
        # In-process relay connector: enroll at the agora, tunnel to the lesche
        # -- both via the host-published ports (host network), not compose DNS.
        # KALLIP_TAGMA_RELAY_ENROLLMENT_CODE comes from .env (minted after
        # signup); until then the tagma runs local-only.
        KALLIP_TAGMA_RELAY_AGORA_URL = "http://127.0.0.1:7100";
        KALLIP_TAGMA_RELAY_LESCHE_URL = "http://127.0.0.1:7200";
        # Seed source for <data_dir>/skills/ on first boot (read-only bundled
        # defaults). Set here rather than baked into the image because dev
        # builds its image ad-hoc via image.contents/useHostStore (not
        # kallip-tagma-image) -- mirrors how PATH is handled above. The store
        # path is reachable directly via the shared host /nix/store.
        KALLIP_SKILLS_SEED = skillsSeed;
        RUST_LOG = "info";
      };
    };
  };
}
