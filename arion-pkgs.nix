# Arion evaluates the compose with this nixpkgs. Mirrors the flake's pkgs
# (same nixpkgs + rust-overlay + aifed overlay) so `pkgs.aifed` resolves
# identically to the flake. Loaded via git+file (not a bare path) so getFlake
# applies fetchGit's VCS filtering and store paths match `nix build .#*`.
let
  flake = builtins.getFlake "git+file://${toString ./.}";
in
import flake.inputs.nixpkgs {
  system = "x86_64-linux";
  overlays = [
    (import flake.inputs.rust-overlay)
    flake.inputs.aifed.overlays.default
  ];
}
