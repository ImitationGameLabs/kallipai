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
# else from its env at runtime. The image carries the command, the compose files
# carry the environment.
pkgs.dockerTools.buildImage {
  name = "kallipai-archeion";
  tag = gitVersion;
  copyToRoot = [
    archeion
    admin
    pkgs.cacert
  ];
  config = {
    Cmd = [ "/bin/kallipai-archeion" ];
    # /bin is the tree copyToRoot merges and the one Cmd names, so the search
    # path and the command point at the same place.
    Env = [ "PATH=/bin" ];
    ExposedPorts = {
      "7100/tcp" = { };
    };
  };
}
