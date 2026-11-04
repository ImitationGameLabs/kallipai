# Host-independent config texts: the docker compose stack mounts them. They
# live here, not inline in the compose files, so the rendered text and the
# assertions about it cannot drift apart.
#
# Nothing in here may read the environment or the store: each value is a plain
# string (or a function returning one), which is what lets a check compare it
# without building anything.
{ lib }:
{
  # The scraper config for the compose prometheus: one 30s job on the
  # gateway's management face, addressed by service name. The compose stack
  # runs no node exporter and no trace store, so this is the whole list there;
  # the NixOS form adds the node exporter and tempo's /metrics beside it.
  prometheus = ''
    global:
      scrape_interval: 30s
    scrape_configs:
      - job_name: kallipai-model-gateway
        static_configs:
          - targets: ["gateway:7500"]
  '';

}
