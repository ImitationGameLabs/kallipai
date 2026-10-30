{
  pkgs,
  common,
  webDist,
}:
let
  inherit (common) gitVersion;

  # What the edge serves, all of it inside the image: the three Caddyfile
  # entries and the shared site they import by name (which resolves beside
  # them), plus the SPA bundle under /srv/web. The entries read the domain,
  # the upstreams and the certificate paths from their environment, so one
  # image serves any deployment of this stack; only the certificates stay a
  # mount, because the host renews them.
  edgeRoot = pkgs.runCommand "kallipai-caddy-web-root" { } ''
    mkdir -p $out/etc/caddy $out/srv/web
    cp ${../../../compose/site.caddy} $out/etc/caddy/site.caddy
    cp ${../../../compose/Caddyfile} $out/etc/caddy/Caddyfile
    cp ${../../../compose/Caddyfile.plain} $out/etc/caddy/Caddyfile.plain
    cp ${../../../compose/Caddyfile.vite} $out/etc/caddy/Caddyfile.vite
    cp -r ${webDist}/. $out/srv/web/
  '';
in
# The edge image: caddy, a shell with wget for the status probes, and the site
# with its four configurations (one body, three entries). The entry is picked at
# run time by the compose `command:`, so dev serves the host's vite server and
# production serves the bundle, from one image. Endpoints change with the
# environment, the domain changes with a restart, and the bundle changes with
# the flake revision.
pkgs.dockerTools.buildImage {
  name = "kallipai-caddy-web";
  tag = gitVersion;
  # busybox gives this image a shell and wget: scripts/compose-status.sh asks
  # the application services from inside this container when the edge runs, and
  # from a one-shot container of this image when it does not. The application
  # images carry neither, which is why the probes come from here.
  copyToRoot = [
    pkgs.caddy
    pkgs.busybox
    edgeRoot
  ];
  config = {
    Cmd = [
      "${pkgs.caddy}/bin/caddy"
      "run"
      "--config"
      "/etc/caddy/Caddyfile"
      "--adapter"
      "caddyfile"
    ];
    # caddy keeps its autosave and certificate cache under these. Nothing mounts
    # them and nothing has to: the config comes from the environment and the
    # cache holds nothing worth keeping, so they live in the container layer.
    Env = [
      "XDG_CONFIG_HOME=/config"
      "XDG_DATA_HOME=/data"
    ];
    ExposedPorts = {
      "80/tcp" = { };
      "443/tcp" = { };
    };
  };
}
