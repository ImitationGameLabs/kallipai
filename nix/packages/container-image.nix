{
  pkgs,
  common,
  # The crane-built workspace (packages.default), built once and reused.
  workspace,
}:
let
  inherit (common) gitVersion;
  shared = import ./container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    certLinks
    aifed
    binPath
    ;

  # Stable base layer: the toolset + CA trust store. buildImage forces its
  # copyToRoot into a single layer (unlike streamLayeredImage, which auto-splits
  # the closure into ~100 layers by narSize). This layer is cached across daemon
  # rebuilds because it only changes when the toolset changes.
  base = pkgs.dockerTools.buildImage {
    name = "just-agent-base";
    tag = gitVersion;
    copyToRoot = [
      toolEnv
      pkgs.cacert
      certLinks
    ];
  };
in
# Daemon layer: just the workspace binaries + aifed (the daemon's intended
# process-level shell-out dep; runtime adoption pending), stacked on the base.
# This is the only layer rebuilt when Rust source changes.
pkgs.dockerTools.buildImage {
  name = "just-agent";
  tag = gitVersion;
  fromImage = base;
  copyToRoot = [
    workspace
    aifed
  ];
  config = {
    Env = [
      "PATH=${binPath}"
      "HOME=/var/lib/just-agent"
      "JUST_AGENT_DATA_DIR=/var/lib/just-agent"
      "JUST_AGENT_DAEMON_ADDR=0.0.0.0:3000"
      "RUST_LOG=info"
    ];
    Cmd = [ "${workspace}/bin/just-agent-daemon" ];
    ExposedPorts = {
      "3000/tcp" = { };
    };
    Volumes = {
      "/var/lib/just-agent" = { };
    };
  };
}
