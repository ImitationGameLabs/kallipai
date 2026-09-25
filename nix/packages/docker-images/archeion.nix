{
  pkgs,
  common,
  archeion,
  admin,
}:
let
  inherit (common) gitVersion;
in
# The minimal archeion image: the binary + the CA trust store + the `kallipai-admin`
# CLI for in-container operator tasks. No shell toolset; archeion reads everything
# else from its env at runtime. The compose service (compose/prod/polis.nix)
# supplies the command + environment.
pkgs.dockerTools.buildImage {
  name = "kallipai-archeion";
  tag = gitVersion;
  copyToRoot = [
    archeion
    admin
    pkgs.cacert
  ];
  config = {
    Cmd = [ "${archeion}/bin/kallipai-archeion" ];
    Env = [ "PATH=${admin}/bin" ];
    ExposedPorts = {
      "7100/tcp" = { };
    };
  };
}
