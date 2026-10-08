{
  pkgs,
  common,
  keryx,
}:
let
  inherit (common) gitVersion;
  # The keryx builds a reqwest Client at startup (HttpControlPlane -> archeion
  # /internal); rustls-platform-verifier loads the system trust store eagerly at
  # .build(), so without the CA bundle at the Debian/RHEL standard paths the
  # keryx panics "No CA certificates were loaded from the system" -- even though
  # its calls are plain HTTP. The shared `cacert` wrapper provides both the
  # cacert bundle and those standard-path symlinks (see container-shared.nix).
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared) cacert;
in
# The minimal keryx image: just the binary + the CA trust store. The keryx is
# a pure HTTP service (axum) like the archeion -- no shell-out toolset, no
# baked env (it reads everything from its env at runtime). The compose service
# (compose/dev/polis.nix) supplies the command + environment. A separate image
# from the lesche so the two relays rebuild and redeploy independently.
pkgs.dockerTools.buildImage {
  name = "kallipai-keryx";
  tag = gitVersion;
  copyToRoot = [
    keryx
  ]
  ++ cacert;
  config = {
    Cmd = [ "${keryx}/bin/kallipai-keryx" ];
    ExposedPorts = {
      "7210/tcp" = { };
    };
  };
}
