{
  pkgs,
  common,
  archeion,
  lesche,
  files,
  keryx,
  model-gateway,
  instances,
}:
let
  inherit (common) gitVersion;
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared) cacert;

  # One merged /bin for the six platform services, so the compose command
  # lines can stay bare names (archeion, lesche, ...) exactly as they read in
  # compose.yaml. A buildEnv is what makes that one directory: the per-crate
  # binaries each carry their own /bin.
  bin = pkgs.buildEnv {
    name = "kallipai-dev-platform-bin";
    paths = [
      archeion
      lesche
      files
      keryx
      model-gateway
      instances
    ];
    pathsToLink = [ "/bin" ];
  };
in
# The dev platform image: no store closure of its own, only symlinks into
# /nix/store plus the merged /bin above. `includeStorePaths = false` is what
# makes that true -- dockerTools copies the closure of every `contents` entry
# by default, which would bake a per-rebuild store copy into the layers.
# compose.dev.yaml bind-mounts the host store read-only at /nix/store, so the
# image runs whatever the checkout built without that copy: that is what makes
# `compose-up.sh` cheap while the prod images stay self-contained.
pkgs.dockerTools.streamLayeredImage {
  name = "kallipai-dev";
  tag = gitVersion;
  includeStorePaths = false;
  contents = [
    bin
  ]
  ++ cacert;
  config = {
    # The compose services name their own command; this default is only what
    # an operator gets from `docker run` on the image by itself.
    Cmd = [ "${bin}/bin/kallipai-archeion" ];
    Env = [ "PATH=${bin}/bin" ];
  };
}
