{
  pkgs,
  common,
  agora,
  admin,
}:
let
  inherit (common) gitVersion;
in
# The minimal agora image: the binary + the CA trust store + the `kallip-admin`
# CLI for in-container operator tasks. No shell toolset; agora reads everything
# else from its env at runtime. The compose service (compose/prod/agora.nix)
# supplies the command + environment.
pkgs.dockerTools.buildImage {
  name = "kallip-agora";
  tag = gitVersion;
  copyToRoot = [
    agora
    admin
    pkgs.cacert
  ];
  config = {
    Cmd = [ "${agora}/bin/kallip-agora" ];
    Env = [ "PATH=${admin}/bin" ];
    ExposedPorts = {
      "7100/tcp" = { };
    };
  };
}
