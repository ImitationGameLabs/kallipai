# The kallip-web static site (SvelteKit adapter-static SPA), built with the
# repo's deno-first toolchain inside the sandbox.
#
# Two derivations, per the standard Nix pattern for npm-style dependency
# trees (cf. fetchNpmDeps): a fixed-output "deps" derivation runs
# `deno install` (networked sandbox, output pinned by hash against
# deno.lock) and a pure build derivation reuses its node_modules offline.
#
# The deps derivation unpacks only the workspace manifests for the
# deno install resolution, so a deno.lock change re-locks the hash
# while a source-only edit cannot retrigger the install.
{
  pkgs,
  deno,
  # The repository source (flake `self`), cleaned of gitignored build state.
  src,
  # One universal dist: deployment values (domain, TLS shape, offline
  # login) are runtime config — see the web app's /config.js and the
  # NixOS module's services.kallipai.web.runtimeConfig option.
}:
let
  # The web build's read closure as a single-level allowlist: the root
  # manifests the deno workspace resolves, tsconfig.base.json (which the
  # library packages' tsconfigs extend and vite/esbuild follows while
  # transforming them), the seven package subtrees the bundle imports
  # (kallip-web -> kallip-ui -> {common, kallip-client, archeion/lesche/
  # files clients}), and the manifests of the three app packages whose
  # sources the web build never reads -- deno install resolves the whole
  # workspace, so their package.json rides along. Excluding the rest
  # (Rust crates, docs, nix expressions, the kallip-app, kallip-direct,
  # and kallip-site trees) keeps unrelated edits from re-materializing the
  # chain. Build state never enters src anyway: the flake source only
  # carries git-tracked files.
  filteredSrc =
    let
      subtreeRoots = [
        "packages/kallip-web"
        "packages/kallip-ui"
        "packages/kallip-common"
        "packages/kallip-client"
        "packages/kallip-archeion-client"
        "packages/kallip-lesche-client"
        "packages/kallip-files-client"
      ];
      exactEntries = subtreeRoots ++ [
        # The parent directory is allow-listed for reachability: a
        # filter false on a directory prunes the whole subtree, so the
        # entries below are reachable only if "packages" admits it.
        # True merely descends; children are still filtered one by one.
        "packages"
        "deno.json"
        "deno.lock"
        "package.json"
        "tsconfig.base.json"
        "packages/kallip-app"
        "packages/kallip-direct"
        "packages/kallip-site"
        "packages/kallip-app/package.json"
        "packages/kallip-direct/package.json"
        "packages/kallip-site/package.json"
      ];
    in
    pkgs.lib.cleanSourceWith {
      inherit src;
      filter =
        path: type:
        let
          relPath = pkgs.lib.removePrefix (toString src + "/") (toString path);
        in
        builtins.elem relPath exactEntries
        || builtins.any (root: pkgs.lib.hasPrefix (root + "/") relPath) subtreeRoots;
    };

  # The deps layer's input: the workspace manifests and nothing else.
  # deno install resolves the module graph from the root manifests and
  # the per-package manifests, never from sources, so pinning this
  # derivation's input to those files keeps a source-only edit from
  # re-running the install on a cold store or in CI. Source edits
  # retrigger the dist build below, never this one.
  manifestsSrc =
    let
      # Directories are admitted for reachability: a filter false on a
      # directory prunes the whole subtree, so each manifest file below
      # is reachable only if its ancestors are listed too.
      manifests = [
        "deno.json"
        "deno.lock"
        "package.json"
        "packages"
        "packages/kallip-web"
        "packages/kallip-web/package.json"
        "packages/kallip-ui"
        "packages/kallip-ui/package.json"
        "packages/kallip-common"
        "packages/kallip-common/package.json"
        "packages/kallip-client"
        "packages/kallip-client/package.json"
        "packages/kallip-archeion-client"
        "packages/kallip-archeion-client/package.json"
        "packages/kallip-lesche-client"
        "packages/kallip-lesche-client/package.json"
        "packages/kallip-files-client"
        "packages/kallip-files-client/package.json"
        "packages/kallip-app"
        "packages/kallip-app/package.json"
        "packages/kallip-direct"
        "packages/kallip-direct/package.json"
        "packages/kallip-site"
        "packages/kallip-site/package.json"
      ];
    in
    pkgs.lib.cleanSourceWith {
      inherit src;
      filter =
        path: type:
        let
          relPath = pkgs.lib.removePrefix (toString src + "/") (toString path);
        in
        builtins.elem relPath manifests;
    };

  # The dist derivation unpacks the full cleaned source tree; the deps
  # derivation unpacks only the manifests tree above.
  nodeModules = pkgs.stdenvNoCC.mkDerivation {
    pname = "kallip-web-node-modules";
    version = "0.0.1";

    impureEnvVars = pkgs.lib.fetchers.proxyImpureEnvVars;

    outputHashMode = "recursive";
    outputHash = "sha256-8kfPaG5iJz5zI1xSayD4LTsraB+QKkCPWnEykWirg34=";
    outputHashAlgo = "sha256";

    nativeBuildInputs = [ deno ];

    src = manifestsSrc;
    # The output is an intermediate: keep the link graph exactly as deno
    # wrote it (fixup's shebang patching would materialize .bin symlinks).
    dontFixup = true;

    buildPhase = ''
      export HOME=$TMPDIR
      # hoisted: npm's classic flat layout, which the bundler's node-style
      # dependency resolution handles without deno-specific link magic.
      deno install --frozen --node-modules-linker=hoisted
      # The paraglide messages are compiled in the dist derivation, not
      # here: a fixed-output derivation reruns only when its declared
      # hash changes, so i18n edits would silently keep the stale
      # messages. Compiling downstream puts them in the dist derivation's
      # regular input graph instead.
    '';

    installPhase = ''
      mkdir $out
      cp -a node_modules $out/node_modules
    '';
  };
in
pkgs.stdenvNoCC.mkDerivation {
  pname = "kallip-web-dist";
  version = "0.0.1";

  src = filteredSrc;

  nativeBuildInputs = [ deno ];

  configurePhase = ''
    export HOME=$TMPDIR
    cp -a ${nodeModules}/node_modules node_modules
    chmod -R u+w node_modules
    # Point the workspace entries back at this tree's live sources (the
    # copies from the deps derivation point into its own store path).
    mkdir -p node_modules/@kallipai
    for pkg in packages/*; do
      name="$(basename "$pkg")"
      rm -rf "node_modules/@kallipai/$name"
      ln -s "../../$pkg" "node_modules/@kallipai/$name"
    done
    # .bin launchers are plain shims whose relative imports only resolve at
    # the package's real location; re-point the one the build invokes.
    ln -sf ../vite/bin/vite.js node_modules/.bin/vite
  '';

  buildPhase = ''
    export HOME=$TMPDIR
    # Compile the paraglide messages first: the inlang plugins load from
    # this tree's node_modules per the project's settings.json, and the
    # compile sits in this derivation's regular input graph, so i18n
    # edits retrigger the bundle.
    (cd packages/kallip-web
    deno run -A --frozen ../../node_modules/@inlang/paraglide-js/bin/run.js compile \
      --project ../kallip-ui/i18n/project.inlang \
      --strategy cookie preferredLanguage baseLocale \
      --emit-ts-declarations \
      --output-structure message-modules \
      --outdir ../kallip-ui/src/paraglide
    )
    deno task build -- --configLoader runner
  '';

  installPhase = ''
    cp -r packages/kallip-web/build $out
  '';
}
