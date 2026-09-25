# Internal helpers shared by the NixOS module and its checks. Not a
# flake-level output: the module is the interface; these exist so the
# module and the checks bake sites identically. System-agnostic by
# construction: anything that needs a package set takes `pkgs` as an
# explicit argument.
{
  # Overlay the runtime config payload onto the kallipai-web bundle: the
  # bundle's files with config.js replaced by the payload (a
  # window.KALLIPAI_CONFIG assignment). The payload carries the user's
  # keys only: keys left unset fall through to the app-side derivation,
  # whose defaults are documented in the shipped config.js. The file is
  # served publicly, so keep secrets out of runtimeConfig.
  bakeRuntimeConfig =
    {
      pkgs,
      package,
      runtimeConfig,
    }:
    let
      configJs = pkgs.writeText "kallipai-config.js" "window.KALLIPAI_CONFIG = ${builtins.toJSON runtimeConfig};";
    in
    pkgs.runCommand "kallipai-web-site" { } ''
      mkdir $out
      cp -r ${package}/. $out/
      cp -f ${configJs} $out/config.js
    '';
}
