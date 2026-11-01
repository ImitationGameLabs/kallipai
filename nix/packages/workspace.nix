{
  pkgs,
  sharedSkills,
  common,
}:
let
  inherit (common)
    craneLib
    commonArgs
    cargoArtifacts
    ;

  # One crane build recipe for a chosen cargo command, on the shared workspace
  # deps cache. `workspace` builds every binary in one derivation; `archeion` /
  # `tagma` build per-crate subsets so the purpose-built docker images carry
  # only the binaries they need (lighter closures -- see docker-images/).
  #
  # NB: doCheck = false so `nix build .#default` doesn't run `cargo test` (whose
  # sandbox-env deps -- CA roots, pgrep/kill -- would pollute the package).
  # NB: do NOT hoist doCheck into commonArgs. crane's cargoNextest and
  # buildDepsOnly both do `args.doCheck or true`, so a shared doCheck = false
  # would silently skip nextest's checkPhase AND drop buildDepsOnly's dev-dep
  # caching. Keep it on this buildPackage call only.
  buildCrate =
    cargoBuildCommand:
    craneLib.buildPackage (
      commonArgs
      // {
        inherit cargoArtifacts cargoBuildCommand;
        doCheck = false;
      }
    );

  # Shell completions for the two operator CLIs. kallipctl takes the
  # dynamic hooks (COMPLETE=$shell, clap_complete CompleteEnv): its slug
  # candidates are runtime daemon queries, and the AOT
  # scripts the hidden generate verb emits bake the word lists at build
  # time, so the dynamic completer never runs there. kallipai-admin has
  # no dynamic candidates and keeps the AOT form. The three shells with
  # standard profile locations install via installShellFiles; all five
  # land under share/kallipai/completions. Only the admin's AOT scripts
  # are portable by copy; the dynamic hooks bake this store path, so
  # other hosts source them straight from the binary.
  genCompletions = emit: bins: ''
    for binname in ${pkgs.lib.concatStringsSep " " bins}; do
      for shell in bash zsh fish elvish powershell; do
        mkdir -p $out/share/kallipai/completions/$binname
        ${emit} > $out/share/kallipai/completions/$binname/$shell
      done
      installShellCompletion --cmd $binname \
        --bash $out/share/kallipai/completions/$binname/bash \
        --zsh $out/share/kallipai/completions/$binname/zsh \
        --fish $out/share/kallipai/completions/$binname/fish
    done
  '';
  # The two emit forms: the AOT verb every CLI carries, and the dynamic
  # CompleteEnv hooks (the COMPLETE env var makes the binary print the
  # hook script instead of running the CLI).
  genAot = genCompletions "$out/bin/$binname generate $shell";
  genDynamic = genCompletions "COMPLETE=$shell $out/bin/$binname";

  # The skills-seed default for `kallipai-tagma`: applied by every shape that puts
  # the binary on PATH (`workspace` and `host` below).
  # --set-default exports KALLIPAI_SKILLS_SEED only when unset, so an explicit env
  # value (or the container image Env) still wins. Only kallipai-tagma reads the
  # seed -- the agent-side kallip CLI sharing its bin/ directory never does, so it
  # stays unwrapped.
  seedWrapper = ''
    wrapProgram $out/bin/kallipai-tagma \
      --set-default KALLIPAI_SKILLS_SEED ${sharedSkills}/share/kallipai/skills
  '';
  # `host` carries exactly these six binaries: the daemon family, the tagma
  # pair, and `kallipai-run` for driving a local tagma agent. The polis
  # services, the cron pair (no unit or compose service runs them) and the
  # telemetry pruner (the dev compose obs stack's retention tool) stay off --
  # each has its own package. Changing that set changes this line with it.
  # Plain ls skips the dot entries the link carries beside the six names
  # (the wrapper's .kallipai-tagma-wrapped), so this reads the real set.
  hostBinCheck = ''
    export LC_ALL=C
    want="kallip kallipai-daemon kallipai-daemon-spawn kallipai-run kallipai-tagma kallipctl"
    got=$(ls $out/bin | sort | paste -sd " " -)
    if [ "$got" != "$want" ]; then
      echo "kallipai-host bin set drifted: got [$got] want [$want]" >&2
      exit 1
    fi
  '';
in
rec {
  # Recursive on purpose: the linked `host` package below references the
  # per-crate builds beside it.

  # The full workspace: every kallipai binary. This is `packages.default`,
  # the single source of truth consumed by the tarball and the
  # integration-test image, and the flake exposes it by name
  # (`packages.workspace`). The NixOS module installs the
  # narrower `kallipai-host` instead: PATH there is for people and their agents,
  # while every polis service runs from its own unit's explicit store path. Both
  # shapes apply the skills-seed wrapper bound above, so `kallipai-tagma`
  # carries the seed default wherever it lands on PATH.
  workspace = (buildCrate "cargo build --release").overrideAttrs (old: {
    nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
      pkgs.makeWrapper
      pkgs.installShellFiles
    ];
    postInstall =
      (old.postInstall or "") + seedWrapper + genDynamic [ "kallipctl" ] + genAot [ "kallipai-admin" ];
  });

  # The shape the NixOS module installs: the binaries the daemon unit and the
  # people on the box need, linked out of the per-crate builds below. The polis
  # services stay out -- their units run their own packages' store paths.
  host = pkgs.symlinkJoin {
    name = "kallipai-host";
    paths = [
      daemon
      daemon-spawn
      ctl
      tagma
      run
    ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = seedWrapper + hostBinCheck;
  };
  # The archeion control-plane server (pure HTTP/Postgres; no shell-out deps).
  archeion = buildCrate "cargo build --release -p kallipai-archeion";
  # The headless archeion admin CLI (HTTP client; runs on the operator host). A
  # separate attr so it can be built/deployed without the server. The
  # archeion image does carry this binary via copyToRoot, so the admin
  # tool -- and its completion scripts -- ship inside the image too;
  # operators run it against any reachable archeion.
  admin = (buildCrate "cargo build --release -p kallipai-admin").overrideAttrs (old: {
    nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.installShellFiles ];
    postInstall = (old.postInstall or "") + genAot [ "kallipai-admin" ];
  });
  # The lesche relay (rooms, direct sessions, the agent SSE and presence;
  # no shell-out deps). Its own image so the archeion and lesche services
  # deploy independently -- see
  # nix/packages/docker-images/lesche.nix.
  lesche = buildCrate "cargo build --release -p kallipai-lesche";
  # The keryx relay (tagma-link and user-DM surface; pure HTTP, no
  # database, no shell-out deps). Its own image -- see
  # nix/packages/docker-images/keryx.nix.
  keryx = buildCrate "cargo build --release -p kallipai-keryx";
  # The files transfer service (content-addressed blobs, ACL'd spaces;
  # pure HTTP/Postgres, no shell-out deps). Its own image so it deploys
  # independently of the archeion/lesche pair -- see
  # nix/packages/docker-images/files.nix.
  files = buildCrate "cargo build --release -p kallipai-files";
  # The host/"tagma" side: the tagma service (agent host + in-process relay
  # connector) and the `kallip` CLI (whose `lesche send` subcommand the agent
  # invokes to address the user) share most of their closure, so one build
  # beats many. The container image (docker-images/tagma.nix) and the dev
  # compose inject KALLIPAI_SKILLS_SEED explicitly, so this build stays bare
  # -- `workspace` and `host` apply the seed wrapper to the linked binary.
  # Excludes archeion.
  tagma = buildCrate "cargo build --release -p kallipai-tagma -p kallip";
  # The runner that drives a local tagma agent and reports its runtime
  # telemetry: the operator/CI side of a daemon box -- see
  # docs/en/reference/kallipai-run.md.
  run = buildCrate "cargo build --release -p kallipai-run";
  # The timer/notification daemon: fires schedules and injects them into agent
  # conversations via the tagma HTTP API. Separate attrs for the daemon and its
  # management CLI so each can be built/deployed independently.
  cron-daemon = buildCrate "cargo build --release -p kallipai-cron-daemon";
  cron = buildCrate "cargo build --release -p kallipai-cron";
  # Local daemon family: the stateless instance manager, its
  # kallipctl CLI, and the setuid-candidate spawn helper -- separate attrs
  # so the daemon (operator host) and helper (root-owned install path)
  # never share a deployment unit.
  daemon = buildCrate "cargo build --release -p kallipai-daemon";
  ctl = (buildCrate "cargo build --release -p kallipctl").overrideAttrs (old: {
    nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.installShellFiles ];
    postInstall = (old.postInstall or "") + genDynamic [ "kallipctl" ];
  });
  daemon-spawn = buildCrate "cargo build --release -p kallipai-daemon-spawn";
  # The HTTP front door of the daemon family: proxies the bare resource
  # to the daemon's UDS socket -- the only networked door the family
  # exposes, with its own host guard and auth modes.
  instances = buildCrate "cargo build --release -p kallipai-instances";
  # The model gateway: the profiles distribution face plus the
  # credential-injecting forwarding face (pure HTTP/Postgres, no
  # shell-out deps). Its own deployment unit -- see
  # services.kallipai.polis.model-gateway in nixos-modules.nix.
  model-gateway = buildCrate "cargo build --release -p kallipai-model-gateway";
  # The local telemetry stack's retention tool (bin + lib): bounds the data
  # directory the dev compose mounts, so the trace store cannot grow without
  # limit. See nix/obs-stack.nix for the stack it serves.
  telemetry-prune = buildCrate "cargo build --release -p kallipai-telemetry-prune";
}
