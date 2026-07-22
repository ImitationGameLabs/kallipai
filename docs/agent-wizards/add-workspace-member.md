# Add Workspace Member Wizard

This document guides AI to add a new workspace member (crate) to the project.

## Instructions for AI

Follow the steps below in order. For each step, **check first** before taking action.

---

## Step 0: Check for Placeholder Crate

Check if a placeholder `hello` crate exists:

```bash
ls -d crates/hello
```

- If exists → Ask user: "Found placeholder `hello` crate. Rename it to `<name>` instead of creating new?"
  - If yes → Rename `crates/hello` to `crates/<name>`, then proceed to Step 3
  - If no → Proceed to Step 1
- If not exists → Proceed to Step 1

---

## Step 1: Check if Crate Already Exists

Check if `crates/<name>/` directory exists:

```bash
ls -d crates/<name>
```

- If exists → Skip to Step 3
- If not exists → Proceed to Step 2

---

## Step 2: Create Crate with cargo new

Use `cargo new` to create the crate:

**For binary crate:**

```bash
cargo new crates/<name>
```

**For library crate:**

```bash
cargo new crates/<name> --lib
```

This automatically creates:

- `crates/<name>/Cargo.toml`
- `crates/<name>/src/main.rs` (binary) or `crates/<name>/src/lib.rs` (library)

> **Platform crates:** the public-internet relay crates (agora, agora-common,
> lesche) live under `crates/platform/`, not flat. For a new platform-side
> crate, substitute `crates/platform/<name>` for `crates/<name>` throughout
> this wizard (the `cargo new` path and the `Cargo.toml` member +
> `[workspace.dependencies]` path). Core and host-side crates stay flat.

---

## Step 3: Check Root Cargo.toml

Check if the crate is registered in workspace members:

```bash
grep -q 'crates/<name>' Cargo.toml
```

- If found → Skip to Step 4
- If not found → Add to workspace members:

```toml
[workspace]
members = [
  # ... existing members ...
  "crates/<name>",
]
```

---

## Step 4: Nix Registration (automatic)

`nix/common.nix` builds the **entire workspace** at once via
`craneLib.cleanCargoSource` + `buildDepsOnly`, and every check in
`nix/checks.nix` runs at workspace granularity (clippy, doc, fmt, audit,
deny). There is **no per-crate registry** to edit — crane auto-discovers any
crate listed in the root `Cargo.toml` `[workspace] members`.

Once Step 3 is done:

- **Library crate**: nothing more to do here. It is compiled as a dependency of
  the workspace build and does not produce its own package.
- **Binary crate**: it is built as part of the workspace. Register an explicit
  package only if you need a standalone `nix build .#<name>` output — see
  `nix/packages/tarball.nix` for the existing release-tarball pattern.

---

## Step 5: (Optional) Set as Default Package

For binary crates that should be the main package, check and update `nix/packages.nix`:

```nix
default = packages.<name>;
```

---

## Step 6: (Optional) Add Native Dependencies

If the crate requires system libraries (e.g., openssl, sqlite), add to `nix/common.nix`:

```nix
commonArgs = {
  # ...
  buildInputs = with pkgs; [
    openssl
    # Add other system libraries here
  ];
};
```

---

## Quick Reference

| Crate Type | cargo new flag | Nix registration           | Generates package                                     |
| ---------- | -------------- | -------------------------- | ----------------------------------------------------- |
| Binary     | (default)      | automatic (workspace scan) | As part of workspace; explicit package only if needed |
| Library    | `--lib`        | automatic (workspace scan) | No (dep tracking only)                                |

## Verification

After all steps:

1. Format Nix files:
   - Single file: `nixfmt nix/common.nix`
   - All at once: `nixfmt $(find nix/ -name "*.nix") flake.nix`
2. Static analysis: `statix check .` (run from project root)
3. Build verification: `nix build .#<name>` (for binary crates)
4. Cargo check: `cargo check -p <name>`
