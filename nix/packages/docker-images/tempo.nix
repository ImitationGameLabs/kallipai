{ pkgs, common }:
let
  inherit (common) gitVersion;
in
# The trace store image: the upstream tempo binary. Its config and the trace
# data directory are bind mounts (KALLIPAI_STACK_DIR and TELEMETRY_BASE), so the
# image holds no state and a restart cannot resurrect an old pipeline.
pkgs.dockerTools.buildImage {
  name = "kallipai-tempo";
  tag = gitVersion;
  copyToRoot = [ pkgs.tempo ];
  config = {
    Cmd = [ "${pkgs.tempo}/bin/tempo" ];
    ExposedPorts = {
      "3200/tcp" = { };
      "4317/tcp" = { };
    };
  };
}
