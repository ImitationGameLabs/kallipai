{ pkgs, common }:
let
  inherit (common) gitVersion;
  # nixpkgs' grafana ships a single `grafana` binary that dispatches on a
  # subcommand and needs --homepath; the compose service names the upstream
  # server command (`grafana-server`). This wrapper bridges the two by name,
  # and bakes the homepath so the compose command line stays what an operator
  # reads in Grafana's own docs.
  grafanaServer = pkgs.writeShellScriptBin "grafana-server" ''
    exec ${pkgs.grafana}/bin/grafana server \
      --homepath=${pkgs.grafana}/share/grafana "$@"
  '';
in
# The UI image: the server wrapper plus the nixpkgs grafana closure. Where it
# writes and what it provisions come from the compose service (GF_PATHS_* and
# the two mounts from KALLIPAI_STACK_DIR); the ini itself is a mount, so the
# image never decides the credentials.
pkgs.dockerTools.buildImage {
  name = "kallipai-grafana";
  tag = gitVersion;
  copyToRoot = [
    pkgs.grafana
    grafanaServer
    pkgs.bashInteractive
  ];
  config = {
    Cmd = [ "${grafanaServer}/bin/grafana-server" ];
    Env = [ "PATH=${grafanaServer}/bin:${pkgs.bashInteractive}/bin" ];
    ExposedPorts = {
      "3000/tcp" = { };
    };
  };
}
