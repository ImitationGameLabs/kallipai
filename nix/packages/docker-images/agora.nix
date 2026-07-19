{
  pkgs,
  common,
  agora,
}:
let
  inherit (common) gitVersion;
in
# The minimal agora image: just the binary + the CA trust store (for any
# outbound TLS). No shell toolset, no baked env (agora reads everything from its
# env at runtime). The compose service (nix/prod-composes/agora.nix) supplies
# the command + environment.
pkgs.dockerTools.buildImage {
  name = "kallip-agora";
  tag = gitVersion;
  copyToRoot = [
    agora
    pkgs.cacert
  ];
  config = {
    Cmd = [ "${agora}/bin/kallip-agora" ];
    ExposedPorts = {
      "7100/tcp" = { };
    };
  };
}
