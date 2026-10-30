{
  pkgs,
  common,
  model-gateway,
}:
let
  inherit (common) gitVersion;
  # The gateway builds a reqwest Client at startup (the archeion calls behind
  # its data plane); rustls-platform-verifier loads the system trust store
  # eagerly at .build(), so the CA bundle has to sit at the Debian/RHEL
  # standard paths (see container-shared.nix).
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared) cacert;
in
# The minimal model-gateway image: the binary + the CA trust store. It is a
# pure HTTP service, so the compose service supplies the command and the whole
# environment. Kept separate from the archeion image so the gateway can be
# rebuilt and redeployed on its own.
pkgs.dockerTools.buildImage {
  name = "kallipai-model-gateway";
  tag = gitVersion;
  copyToRoot = [
    model-gateway
  ]
  ++ cacert;
  config = {
    Cmd = [ "/bin/kallipai-model-gateway" ];
    ExposedPorts = {
      "7501/tcp" = { };
      "7500/tcp" = { };
    };
  };
}
