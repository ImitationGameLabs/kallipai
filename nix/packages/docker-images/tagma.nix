{
  pkgs,
  common,
  tagma,
}:
let
  inherit (common) gitVersion;
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    certLinks
    aifed
    binPath
    ;
in
# The tagma image: daemon + herald binaries + the daemon's shell toolset (the
# agent landlock sandbox shells out to bash/coreutils/ripgrep/git/pgrep/kill),
# the CA trust store, and aifed. It carries NO daemon-specific baked env (no
# KALLIP_DAEMON_ADDR/KALLIP_DATA_DIR/...) and NO default Cmd: the compose
# `daemon` and `herald` services each set their own `command` + `environment`,
# so the daemon's flavor cannot leak into the herald (or vice versa). Only PATH
# is baked, since both resolve tools via PATH.
pkgs.dockerTools.buildImage {
  name = "kallip-tagma";
  tag = gitVersion;
  copyToRoot = [
    tagma
    toolEnv
    pkgs.cacert
    certLinks
    aifed
  ];
  config = {
    Env = [ "PATH=${binPath}" ];
    # No Cmd: the compose service supplies the command (kallip-daemon or
    # kallip-herald).
  };
}
