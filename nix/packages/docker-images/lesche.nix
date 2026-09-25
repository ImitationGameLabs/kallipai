{
  pkgs,
  common,
  lesche,
}:
let
  inherit (common) gitVersion;
  # The lesche builds a reqwest Client at startup (HttpControlPlane -> archeion
  # /internal); rustls-platform-verifier loads the system trust store eagerly at
  # .build(), so without the CA bundle at the Debian/RHEL standard paths the
  # lesche panics "No CA certificates were loaded from the system" -- even though
  # its calls are plain HTTP. The shared `cacert` wrapper provides both the
  # cacert bundle and those standard-path symlinks (see container-shared.nix).
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared) cacert;
in
# The minimal lesche image: just the binary + the CA trust store. The lesche is
# a pure HTTP service (axum) like the archeion -- no shell-out toolset, no baked env
# (it reads everything from its env at runtime). The compose service
# (compose/prod/polis.nix, arion-compose.nix dev) supplies the command +
# environment. A separate image from the archeion so the two services can be
# rebuilt/redeployed independently (the point of the control/data-plane split).
pkgs.dockerTools.buildImage {
  name = "kallipai-lesche";
  tag = gitVersion;
  copyToRoot = [
    lesche
  ]
  ++ cacert;
  config = {
    Cmd = [ "${lesche}/bin/kallipai-lesche" ];
    ExposedPorts = {
      "7200/tcp" = { };
    };
  };
}
