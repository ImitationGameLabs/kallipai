{ pkgs }:
let
  # One merged /bin tree put on PATH. The daemon spawns `bash` via PATH
  # resolution (ShellBuilder default) and shells out to `pgrep`/`kill`, so the
  # toolset must live on PATH alongside the workspace binaries. pathsToLink
  # merges every package's /bin into a single ${toolEnv}/bin.
  #
  # Shared by the baked image (nix/packages/container-image.nix) and the dev
  # compose (arion-compose.nix) so the two cannot drift.
  toolEnv = pkgs.buildEnv {
    name = "just-agent-path-env";
    paths = [
      pkgs.bashInteractive
      pkgs.coreutils
      pkgs.findutils
      pkgs.diffutils
      pkgs.gnugrep
      pkgs.gnused
      pkgs.ripgrep
      pkgs.git
      pkgs.procps # pgrep
      pkgs.util-linux # kill
    ];
    pathsToLink = [ "/bin" ];
  };

  # rustls-platform-verifier reads the trust store at standard paths only
  # (ignores SSL_CERT_FILE), so expose the cacert bundle at both the Debian and
  # RHEL conventions.
  #
  # Built as a derivation (not an extraCommands/fakeRootCommands script) so it
  # works uniformly in buildImage.copyToRoot (prod) and image.contents (dev) —
  # buildImage's extraCommands runs without fakeroot and cannot mkdir under the
  # non-writable `etc` that cacert's copyToRoot brings in.
  certLinks = pkgs.runCommand "just-agent-cert-links" { } ''
    mkdir -p $out/etc/ssl/certs $out/etc/pki/tls/certs
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-certificates.crt
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/pki/tls/certs/ca-bundle.crt
  '';

  # aifed: the daemon's intended process-level shell-out dep (runtime adoption
  # pending). Put on PATH so the daemon resolves it by name, like bash/pgrep.
  inherit (pkgs) aifed;

  # The full container PATH, built once so prod and dev cannot drift.
  binPath = "${toolEnv}/bin:${aifed}/bin";
in
{
  inherit
    toolEnv
    certLinks
    aifed
    binPath
    ;
}
