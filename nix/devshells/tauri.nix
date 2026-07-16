# Optional devShell for the kallip-app Android (Tauri mobile) target.
#
# Desktop is intentionally NOT supported here. Tauri's Linux desktop backend
# (WebKitGTK, GTK3) does not handle Wayland fractional scaling: it reports a
# broken devicePixelRatio and renders content tiny on HiDPI fractional displays
# (upstream tauri#5600 / #6224 / #14590). Rather than carry the heavy Linux GUI
# toolchain (webkitgtk / gtk / cairo / ... / pkg-config) to paper over an
# upstream bug, desktop-class use is served by kallip-web in a browser, and this
# shell builds only the Android app. `tauri dev` / `tauri build` (desktop) will
# therefore FAIL on Linux for lack of webkitgtk / pkg-config — that is expected.
#
# To re-enable desktop later: uncomment the `tauriLinuxNative` binding below and
# re-add its `++ lib.optionals pkgs.stdenv.isLinux tauriLinuxNative` append on
# `packages`.
#
# Why separate from devShells.default:
#   The app's Rust lives in packages/kallip-app/src-tauri as a standalone Cargo
#   project (NOT a member of the root workspace) precisely so Tauri's heavy
#   native toolchain + Android SDK never leak into backend builds or the
#   default devShell. This shell is opt-in: `nix develop .#tauri`.
#
#   Mirrors the proven talk-tree config (nix/dev/shell.nix), adapted for kallip:
#   - appCraneLib carries the cross targets the backend toolchain omits: wasm32
#     (shared agora crypto) and the android std targets (tauri android).
{
  pkgs,
  inputs,
  ...
}:

let
  # App-scoped craneLib. Separate from the backend common.craneLib so the
  # backend devShell/release builds never pull the cross toolchain.
  appCraneLib = (inputs.crane.mkLib pkgs).overrideToolchain (
    p:
    p.rust-bin.stable.latest.default.override {
      extensions = [ "rust-src" ];
      # The cross targets are preinstalled here (not via rustup): Tauri would
      # otherwise shell out to `rustup target add`, which is absent under
      # rust-overlay and cannot write the read-only nix store. This is why
      # `tauri android init` needs `--skip-targets-install` and `tauri android
      # build` needs an explicit `--target` (unlike `dev`); see
      # docs/frontend-development.md for the commands.
      targets = [
        # Shared agora crypto, single-source build consumed by web + app.
        "wasm32-unknown-unknown"
        # Android ABIs are restricted to arm64 + x86_64 via the
        # ORG_GRADLE_PROJECT_abiList/archList/targetList env vars below (read
        # by the generated RustPlugin), so gradle only invokes cargo for these
        # two triples. armv7/i686 are never built, so their std is omitted.
        "aarch64-linux-android" # arm64-v8a: real devices
        "x86_64-linux-android" # x86_64 emulator
      ];
    }
  );

  # AGP 8.11 requires build-tools 35.0.0; also used by the aapt2 override below.
  buildToolsVersion = "35.0.0";
  androidComposition = pkgs.androidenv.composeAndroidPackages {
    # Platforms must be preinstalled: the SDK is read-only in the nix store, so
    # anything AGP or the emulator needs but we omit triggers a failed
    # auto-install.
    #   36 covers both the app's compileSdk/targetSdk AND the local `test` AVD
    #   (~/.android/avd), which is rebuilt against this platform so a single
    #   platform covers build + emulator (no separate 34 needed).
    platformVersions = [ "36" ];
    buildToolsVersions = [ buildToolsVersion ];
    # Pin the system image nixpkgs installs so the AVD's package id is fixed.
    # Create the local `test` AVD against it (omit -d and it falls back to a
    # tiny low-res profile):
    #   avdmanager create avd -n test -k "system-images;android-36;google_apis;x86_64" -d pixel_9
    systemImageTypes = [ "google_apis" ];
    abiVersions = [ "x86_64" ];
    includeNDK = true;
    # Emulator + its system image, for `tauri android dev` against an AVD. Heavy
    # (multi-GB); turn off if you only ever build APKs or deploy to a real device.
    includeEmulator = true;
    includeSystemImages = true;
  };

  # Desktop is no longer a target (see the file header). The Linux GUI deps are
  # kept below as a commented recipe: to re-enable, uncomment this binding and
  # re-add `++ lib.optionals pkgs.stdenv.isLinux tauriLinuxNative` to `packages`.
  # tauriLinuxNative = with pkgs; [
  #   webkitgtk_4_1
  #   gtk3
  #   cairo
  #   glib
  #   gdk-pixbuf
  #   pango
  #   librsvg
  #   libayatana-appindicator
  #   xdotool
  #   pkg-config
  # ];
in
appCraneLib.devShell rec {
  packages = with pkgs; [
    rust-analyzer

    # JS build for the app (drives the @tauri-apps/cli via `deno task tauri`).
    deno

    # Android
    androidComposition.androidsdk
    gradle
    jdk
  ];

  # Mirrors the talk-tree env: aapt2 is pinned to the nix-provided build-tools
  # copy so Gradle does not try to download its own (which would fail offline /
  # diverge from the pinned SDK).
  ANDROID_JAVA_HOME = "${pkgs.jdk.home}";
  ANDROID_HOME = "${androidComposition.androidsdk}/libexec/android-sdk";
  ANDROID_NDK_HOME = "${ANDROID_HOME}/ndk-bundle";
  GRADLE_OPTS = "-Dorg.gradle.project.android.aapt2FromMavenOverride=${ANDROID_HOME}/build-tools/${buildToolsVersion}/aapt2";

  # Restrict the Android ABIs Tauri builds to arm64 + x86_64 (real devices +
  # emulator), dropping legacy armeabi-v7a (32-bit ARM) and x86 (32-bit
  # emulator). Gradle maps ORG_GRADLE_PROJECT_<name> env vars to project
  # properties, which the generated RustPlugin reads via findProperty — so this
  # applies to every gradle run including `tauri android init`'s verification,
  # without editing the generated (re-init-clobberable) gradle.properties.
  # The three lists are index-aligned: abi <-> arch <-> rust target.
  ORG_GRADLE_PROJECT_abiList = "arm64-v8a,x86_64";
  ORG_GRADLE_PROJECT_archList = "arm64,x86_64";
  ORG_GRADLE_PROJECT_targetList = "aarch64,x86_64";
}
