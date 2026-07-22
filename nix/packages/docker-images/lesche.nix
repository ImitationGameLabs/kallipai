{
  pkgs,
  common,
  lesche,
}:
let
  inherit (common) gitVersion;
  # certLinks exposes the cacert bundle at the Debian + RHEL standard paths that
  # rustls-platform-verifier reads. The lesche builds a reqwest Client at startup
  # (HttpControlPlane -> agora /internal); the platform verifier loads the system
  # trust store eagerly at .build(), so without these symlinks the lesche panics
  # "No CA certificates were loaded from the system" -- even though its calls are
  # plain HTTP. (cacert alone lands at $out/etc/ssl/certs/ca-bundle.crt, which the
  # verifier does NOT read.)
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared) certLinks;
in
# The minimal lesche image: just the binary + the CA trust store. The lesche is
# a pure HTTP service (axum) like the agora -- no shell-out toolset, no baked env
# (it reads everything from its env at runtime). The compose service
# (nix/prod-composes/agora.nix, arion-compose.nix dev) supplies the command +
# environment. A separate image from the agora so the two services can be
# rebuilt/redeployed independently (the point of the control/data-plane split).
pkgs.dockerTools.buildImage {
  name = "kallip-lesche";
  tag = gitVersion;
  copyToRoot = [
    lesche
    pkgs.cacert
    certLinks
  ];
  config = {
    Cmd = [ "${lesche}/bin/kallip-lesche" ];
    ExposedPorts = {
      "7200/tcp" = { };
    };
  };
}
