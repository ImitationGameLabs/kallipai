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
    cacert
    aifed
    binPath
    skillsSeed
    ;
in
# The tagma image: the tagma binary (agent host + in-process relay connector),
# the `kallip` CLI (whose `reply` subcommand the agent invokes to address the
# user), and the tagma's shell toolset (the agent landlock sandbox shells out to
# bash/coreutils/ripgrep/git/pgrep/kill), the CA trust store, and aifed. It
# carries NO tagma-specific baked env (no KALLIPAI_TAGMA_ADDR/KALLIPAI_TAGMA_SLUG/...)
# and NO default Cmd: the compose `tagma` service sets its own `command` +
# `environment`. Only PATH and KALLIPAI_SKILLS_SEED are baked: both are store
# paths intrinsic to the build (identical across deploys), and the tagma + its
# agent shells resolve tools (and `kallip lesche send`) via PATH while the
# tagma seeds skill_dir() (KALLIPAI_SKILLS_ROOT if set, else <data_dir>/skills/)
# from KALLIPAI_SKILLS_SEED on first boot. The
# shared-skills store path enters the closure via the env-var interpolation, so
# it is NOT added to copyToRoot (read directly from /nix/store).
pkgs.dockerTools.buildImage {
  name = "kallipai-tagma";
  tag = gitVersion;
  copyToRoot = [
    tagma
    toolEnv
    aifed
  ]
  ++ cacert;
  config = {
    Env = [
      "PATH=${tagma}/bin:${binPath}"
      "KALLIPAI_SKILLS_SEED=${skillsSeed}"
    ];
    # No Cmd: the compose service supplies the command (kallipai-tagma).
  };
}
