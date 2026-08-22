// Guard for the cross-package twin routes: the two host apps (kallip-app,
// kallip-web) must keep their route trees byte-identical so the shared pages
// cannot silently drift apart (they are maintained by hand-discipline today).
// The file name breaks the gate.ts <-> gate_test.ts pairing convention on
// purpose: the test target is the twin route trees themselves, not a module
// in this package. Monorepo assumption: resolving four levels up reaches
// packages/, where both hosts live — if the workspace is ever split into
// repos this test fails, forcing the ownership question to be re-decided.
import { assert } from "@std/assert";

const APP_ROUTES = new URL(
  "../../../../kallip-app/src/routes/",
  import.meta.url,
);
const WEB_ROUTES = new URL(
  "../../../../kallip-web/src/routes/",
  import.meta.url,
);

// Only the root +layout.svelte differs (host environment adapters). Its twin
// +layout.ts is identical on both hosts and therefore stays guarded.
const EXEMPTED = new Set(["+layout.svelte"]);

function collectTwinFiles(dir: URL, prefix = ""): Map<string, Uint8Array> {
  const files = new Map<string, Uint8Array>();
  for (const entry of Deno.readDirSync(dir)) {
    const rel = prefix + entry.name;
    if (entry.isDirectory) {
      for (const [path, bytes] of collectTwinFiles(
        new URL(`${entry.name}/`, dir),
        `${rel}/`,
      )) {
        files.set(path, bytes);
      }
    } else if (entry.isFile) {
      files.set(rel, Deno.readFileSync(new URL(entry.name, dir)));
    } else {
      // A symlink or any other entry type would be silently skipped and
      // punch a hole in the guard; refuse instead.
      throw new Error(`unexpected routes entry type at ${rel}`);
    }
  }
  return files;
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

Deno.test(
  "twin routes stay byte-identical across hosts",
  // The tree walk reads outside this package; scope the grant to the two
  // routes dirs instead of opening up the whole filesystem. Run via
  // `deno task test`: a bare `deno test` cannot escalate this grant.
  { permissions: { read: [APP_ROUTES, WEB_ROUTES] } },
  () => {
    const app = collectTwinFiles(APP_ROUTES);
    const web = collectTwinFiles(WEB_ROUTES);

    const onlyApp = [...app.keys()].filter((p) => !web.has(p));
    const onlyWeb = [...web.keys()].filter((p) => !app.has(p));
    assert(
      onlyApp.length === 0 && onlyWeb.length === 0,
      `route trees diverged: only in kallip-app [${onlyApp.join(
        ", ",
      )}], only in kallip-web [${onlyWeb.join(
        ", ",
      )}] — add the missing twin page or extend EXEMPTED deliberately`,
    );

    const drifted: string[] = [];
    for (const [path, appBytes] of app) {
      if (EXEMPTED.has(path)) continue;
      const webBytes = web.get(path)!;
      if (!bytesEqual(appBytes, webBytes)) {
        drifted.push(
          `${path} (${appBytes.length} vs ${webBytes.length} bytes)`,
        );
      }
    }
    assert(
      drifted.length === 0,
      `twin files drifted apart — sync the twin or extend exemptions deliberately (also check EOL / git autocrlf): ${drifted.join(
        "; ",
      )}`,
    );

    // Exemption health: an exempted file must exist on both hosts and actually
    // differ, so the list cannot silently rot when hosts converge or delete it.
    for (const path of EXEMPTED) {
      const a = app.get(path);
      const w = web.get(path);
      assert(
        a !== undefined && w !== undefined,
        `exempted ${path} is missing on one host — remove it from EXEMPTED`,
      );
      assert(
        !bytesEqual(a!, w!),
        `exempted ${path} is now identical on both hosts — remove it from EXEMPTED`,
      );
    }
  },
);
