{
  pkgs,
  common,
  instances,
}:
let
  inherit (common) gitVersion;
  # instances talks to the tagma daemon over a unix socket and to the archeion
  # over HTTP; the HTTP client needs the CA bundle at the standard paths (see
  # container-shared.nix).
  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared) cacert;
in
# The minimal instances image: the binary + the CA trust store. The socket it
# proxies to is a host bind mount named in the compose service, never baked
# into the image.
pkgs.dockerTools.buildImage {
  name = "kallipai-instances";
  tag = gitVersion;
  copyToRoot = [
    instances
  ]
  ++ cacert;
  config = {
    Cmd = [ "/bin/kallipai-instances" ];
    ExposedPorts = {
      "7300/tcp" = { };
    };
  };
}
