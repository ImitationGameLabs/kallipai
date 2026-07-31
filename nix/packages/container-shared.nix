{ pkgs }:
let
  # One merged /bin tree put on PATH. The tagma spawns `bash` via PATH
  # resolution (ShellBuilder default) and shells out to `pgrep`/`kill`, so the
  # toolset must live on PATH alongside the workspace binaries. pathsToLink
  # merges every package's /bin into a single ${toolEnv}/bin.
  #
  # Carries the container's shell toolset: generic unix utils, dev-facing tools
  # (git, ripgrep, direnv), and nix itself -- the package manager is container
  # base infra for realizing project flakes and agent self-installed tooling via
  # `nix shell`. sed is deliberately omitted (prefer the harness's edit tools).
  #
  # Shared by the tagma docker image (nix/packages/docker-images/tagma.nix) and
  # the dev compose (arion-compose.nix) so the two cannot drift.
  toolEnv = pkgs.buildEnv {
    name = "kallip-path-env";
    paths = [
      pkgs.bashInteractive
      pkgs.coreutils
      pkgs.findutils
      pkgs.diffutils
      pkgs.gnugrep
      pkgs.ripgrep
      pkgs.git
      pkgs.nix
      pkgs.direnv
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
  certLinks = pkgs.runCommand "kallip-cert-links" { } ''
    mkdir -p $out/etc/ssl/certs $out/etc/pki/tls/certs
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-certificates.crt
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/pki/tls/certs/ca-bundle.crt
  '';

  # aifed: the tagma's intended process-level shell-out dep (runtime adoption
  # pending). Put on PATH so the tagma resolves it by name, like bash/pgrep.
  inherit (pkgs) aifed;

  # The full container PATH, built once so prod and dev cannot drift.
  binPath = "${toolEnv}/bin:${aifed}/bin";

  # The CA trust base: pkgs.cacert plus its standard-path symlinks (certLinks
  # above), so rustls-platform-verifier -- which reads only the Debian/RHEL
  # standard paths, ignoring SSL_CERT_FILE and cacert's own $out path -- finds
  # the bundle. A thin wrapper around pkgs.cacert, named after it. Consumed by
  # every service that builds a reqwest/rustls client at startup: lesche (->
  # agora /internal) and the tagma (relay connector). agora is plain HTTP + DB
  # and needs none. Consumers compose it directly into image.contents /
  # copyToRoot rather than via an aggregator, e.g.
  #   copyToRoot = [ tagma toolEnv aifed ] ++ cacert;
  cacert = [
    pkgs.cacert
    certLinks
  ];

  # Curated shared-skill tree (read-only bundled defaults). The tagma copies
  # this into the mutable <data_dir>/skills/ on first boot via the
  # KALLIP_SKILLS_SEED env var below. Same file the flake exposes as
  # `kallip-shared-skills`, so the image/compose and the flake output agree
  # bit-for-bit. let-local: only the derived `skillsSeed` path is consumed by
  # the image/compose; the package itself is not part of the public surface
  # (callers that want the package use the flake output).
  sharedSkills = import ./shared-skills.nix { inherit pkgs; };
  skillsSeed = "${sharedSkills}/share/kallip/skills";
in
{
  inherit
    toolEnv
    cacert
    aifed
    binPath
    skillsSeed
    ;
}
