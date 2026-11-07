// The build-version watch: the shell's own poll for a newer bundle.
//
// SvelteKit writes `_app/version.json` into every build and hands the
// running bundle's id to the app as `$app/environment`'s `version`. In this
// kit version nothing asks for that file on its own (the store's `check()`
// runs only on two error fallbacks), so the shell asks, at a bounded pace,
// and reports one answer.

/** The part of the page's `document` the watch touches. Stated
 *  structurally, not as the DOM type: this package type-checks under
 *  both DOM-lib environments (svelte-check) and Deno's test lib, where
 *  `Document` does not exist (see lib/session/notify.ts for the same seam). */
export interface VersionDocument {
  readonly visibilityState: string;
  addEventListener(type: "visibilitychange", listener: () => void): void;
  removeEventListener(type: "visibilitychange", listener: () => void): void;
}

export interface VersionWatchOptions {
  /** The running bundle's id (`$app/environment`'s `version`). */
  readonly current: string;
  /** Read the deployed id -- `_app/version.json`'s `version` -- or return a
   *  value the watch will reject. A throw is the same as no answer. */
  readonly fetchVersion: () => Promise<unknown>;
  /** Called once, when a usable id differs from the running one. */
  readonly onAvailable: () => void;
  /** How often to ask, in milliseconds. Default one minute. */
  readonly intervalMs?: number;
  /** Where the visibility hook lives. Defaults to `document`. */
  readonly doc?: VersionDocument;
}

/** Longest id the watch will believe: a version is a short token (a git
 *  revision, a build clock), so a longer value is a proxy's error page or a
 *  truncated read rather than a deployment. */
const MAX_ID_LENGTH = 64;

/** Whether a fetched value is a version at all: a non-empty, bounded,
 *  whitespace-free string. */
export function usableVersionId(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= MAX_ID_LENGTH &&
    !/\s/.test(value)
  );
}

/** The page's document, reached without naming a DOM type. */
function browserDocument(): VersionDocument {
  const doc = (globalThis as { document?: VersionDocument }).document;
  if (doc === undefined) {
    throw new Error(
      "startVersionWatch needs a document (call it in the browser)",
    );
  }
  return doc;
}

/** Start watching for a deployed bundle that differs from the running one.
 *
 *  The judgement is "usable and different", not "newer": a rollback serves an
 *  id below the running one, and a page left on a superseded bundle is
 *  exactly the page that should hear about it. The watch reports and stops --
 *  the notice cannot un-happen, and the caller's action (a reload the user
 *  chooses) settles the question anyway.
 *
 *  Returns a stop function; the caller runs it on unmount. Calling it twice
 *  is harmless. */
export function startVersionWatch(opts: VersionWatchOptions): () => void {
  const { current, fetchVersion, onAvailable } = opts;
  const intervalMs = opts.intervalMs ?? 60_000;
  const doc = opts.doc ?? browserDocument();

  let stopped = false;
  let asking = false;

  function stop(): void {
    if (stopped) return;
    stopped = true;
    clearInterval(timer);
    doc.removeEventListener("visibilitychange", onVisibility);
  }

  // One question in flight at a time: a slow answer must not stack the
  // interval's asks behind it.
  async function check(): Promise<void> {
    if (stopped || asking) return;
    asking = true;
    try {
      const id = await fetchVersion();
      if (stopped || !usableVersionId(id) || id === current) return;
      stop();
      onAvailable();
    } catch {
      // A deployment that carries no version file, or a probe that failed,
      // says nothing about the bundle: the next ask tries again.
    } finally {
      asking = false;
    }
  }

  // Back from another tab: the bundle may have been replaced while this page
  // was hidden, and waiting out the interval would leave the notice late
  // exactly when someone is looking at the page.
  function onVisibility(): void {
    if (doc.visibilityState === "visible") void check();
  }

  doc.addEventListener("visibilitychange", onVisibility);
  const timer = setInterval(() => void check(), intervalMs);
  void check();
  return stop;
}
