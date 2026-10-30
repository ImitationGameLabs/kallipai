{ pkgs, common }:
let
  inherit (common) gitVersion;
in
# The collector image: the upstream otelcol-contrib binary and nothing else.
# The config is a bind mount from KALLIPAI_STACK_DIR, so the image carries no
# pipeline of its own and one image serves both the dev and the prod stack.
pkgs.dockerTools.buildImage {
  name = "kallipai-otelcol";
  tag = gitVersion;
  copyToRoot = [ pkgs.opentelemetry-collector-contrib ];
  config = {
    Cmd = [ "${pkgs.opentelemetry-collector-contrib}/bin/otelcol-contrib" ];
    ExposedPorts = {
      "4317/tcp" = { };
      "4318/tcp" = { };
      "13133/tcp" = { };
    };
  };
}
