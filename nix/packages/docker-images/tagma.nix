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
# The tagma image: the tagma binary (agent host + in-process relay connector),
# the `kallip` CLI (whose `reply` subcommand the agent invokes to address the
# user), and the tagma's shell toolset (the agent landlock sandbox shells out to
# bash/coreutils/ripgrep/git/pgrep/kill), the CA trust store, and aifed. It
# carries NO tagma-specific baked env (no KALLIP_TAGMA_ADDR/KALLIP_DATA_DIR/...)
# and NO default Cmd: the compose `tagma` service sets its own `command` +
# `environment`. Only PATH is baked, since the tagma + its agent shells resolve
# tools (and `kallip lesche send`) via PATH.
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
    # No Cmd: the compose service supplies the command (kallip-tagma).
  };
}
