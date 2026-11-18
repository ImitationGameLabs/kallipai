// Pins the two premises the frontend lint gate rests on. `no-sloppy-imports`
// must still fire on a `.js` specifier whose real file is the `.ts` neighbour:
// that rule keeps extensionless and `.js`-named imports out of the tree, and the
// UI package resolving them differently would silence it while `deno task lint`
// stayed green. `no-explicit-any` must still fire on an explicit `any`: the row
// types keep one scoped suppression for Svelte's open `Component` slots, and a
// config that switched the rule off would leave that suppression as the only
// trace while every new `any` passed.
//
// The probe lands inside packages/kallipai-ui/src/lib because the rule reads
// the importer's own package layout. A finally empties all three again
// even when the guard fails; a killed process can still leave probes behind,
// and either probe is exactly the shape a rule rejects: a bare
// `deno lint`, or a guarded run started within the next minute, fails on it,
// while a later guarded run sweeps it first and passes. Clean up after a
// killed run.
//
// The any probe needs no package layout; it sits beside the pair so the sweep
// and the finally clean all three the same way.
//
// The names carry a per-run suffix, so two concurrent lint runs cannot delete
// each other's probe; a whole-tree lint still sees all three while they live.

const repo = `${import.meta.dirname}/../`;
const dir = `${repo}packages/kallipai-ui/src/lib`;
const tag = crypto.randomUUID().slice(0, 8);
const target = `${dir}/__lint_guard_target_${tag}.ts`;
const importer = `${dir}/__lint_guard_importer_${tag}.ts`;
const anyProbe = `${dir}/__lint_guard_any_${tag}.ts`;

const decoder = new TextDecoder();
try {
  // Sweep leftovers from a killed run, but never a peer's live probe: a whole
  // guard run takes seconds, so anything this old was abandoned.
  const cutoff = Date.now() - 60_000;
  for (const entry of Deno.readDirSync(dir)) {
    if (!entry.name.startsWith("__lint_guard_")) continue;
    const path = `${dir}/${entry.name}`;
    // A peer sweeping the same leftover may delete it between these two
    // calls.
    let mtime: number;
    try {
      mtime = Deno.statSync(path).mtime?.getTime() ?? 0;
    } catch {
      continue;
    }
    if (mtime >= cutoff) continue;
    try {
      Deno.removeSync(path);
    } catch {
      // The peer got there first.
    }
  }
  Deno.writeTextFileSync(target, "export const probe = 1;\n");
  Deno.writeTextFileSync(anyProbe, "export const probe: any = 1;\n");
  Deno.writeTextFileSync(
    importer,
    'import { probe } from "./__lint_guard_target_' +
      tag +
      '.js";\n\n' +
      "export const guard = probe;\n",
  );
  const out = new Deno.Command(Deno.execPath(), {
    // clearEnv keeps the child from inheriting the build tree's LD_* overrides,
    // which would point it at libraries this process never linked against.
    clearEnv: true,
    args: ["lint", importer, anyProbe],
    cwd: repo,
    stdout: "piped",
    stderr: "piped",
  }).outputSync();
  const text = decoder.decode(out.stdout) + decoder.decode(out.stderr);
  if (text.trim() === "") {
    console.error(
      `lint rule guard: deno lint produced no output at all (rc=${out.code}), so this run proves nothing about the rule.`,
    );
    Deno.exitCode = 1;
  } else {
    let missing = false;
    if (!text.includes("error[no-sloppy-imports]")) {
      console.error(
        "lint rule guard: error[no-sloppy-imports] did not fire on a .js specifier naming a .ts file, so the lint gate can no longer fail a sloppy import.",
      );
      missing = true;
    }
    if (!text.includes("error[no-explicit-any]")) {
      console.error(
        "lint rule guard: error[no-explicit-any] did not fire on an explicit any, so the lint gate can no longer fail an explicit any.",
      );
      missing = true;
    }
    if (missing) {
      console.error(text.trim());
      Deno.exitCode = 1;
    }
  }
} finally {
  for (const path of [target, importer, anyProbe]) {
    try {
      Deno.removeSync(path);
    } catch {
      // Already gone: the other cleanup still has to run.
    }
  }
}
