{
  pkgs,
  common,
  files,
}:
let
  inherit (common) gitVersion;
  # The files service builds a reqwest Client at startup (the archeion /internal
  # ControlPlane calls); rustls-platform-verifier loads the system trust store
  # eagerly at .build(), so without the CA bundle at the Debian/RHEL standard
  # paths the service panics "No CA certificates were loaded from the system"
  # -- even though its calls are plain HTTP. The shared `cacert` wrapper
  # provides both the cacert bundle and those standard-path symlinks (see
  # container-shared.nix).
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared) cacert;
in
# The minimal files image: just the binary + the CA trust store. The files
# service is a pure HTTP service (axum) like the archeion and the lesche -- no
# shell-out toolset, no baked env (it reads everything from its env at
# runtime). The compose service (compose/prod/polis.nix) supplies the command
# and the environment. A separate image so the three server-side services
# rebuild/redeploy independently.
pkgs.dockerTools.buildImage {
  name = "kallipai-files";
  tag = gitVersion;
  copyToRoot = [
    files
  ]
  ++ cacert;
  config = {
    Cmd = [ "${files}/bin/kallipai-files" ];
    ExposedPorts = {
      "7400/tcp" = { };
    };
  };
}
