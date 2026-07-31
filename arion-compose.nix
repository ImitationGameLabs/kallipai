# Arion auto-discovery shim: arion only auto-loads `arion-compose.nix` at the
# repo root, so this re-exports compose/dev/agora.nix to keep `arion up`
# working. The actual dev agora-side composition lives there.
import ./compose/dev/agora.nix
