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

/** What one ask learned. `kind: "id"` carries the `version` field exactly as
 *  read: a candidate the watch weighs, where one it cannot believe ends the
 *  watch as surely as `absent` does. */
export type VersionAnswer =
  | { readonly kind: "id"; readonly candidate: unknown }
  | { readonly kind: "absent" };

/** The part of a `Response` one ask reads, stated structurally for the same
 *  reason as `VersionDocument`: no DOM or fetch type has to exist. */
export interface VersionResponse {
  readonly status: number;
  readonly ok: boolean;
  text(): Promise<string>;
}

/** One HTTP ask. Defaults to `fetch`; a test passes its own. */
export type VersionFetch = (url: string) => Promise<VersionResponse>;

/** What one response says about the deployment.
 *
 *  The question is whether this deployment publishes a readable version
 *  document, not what number the status carries. A 404 or a 410 says it does
 *  not, and so does a 200 whose body is not JSON: `compose/site.caddy`
 *  answers `try_files {path} /index.html`, so a file the build lacks comes
 *  back as the shell, an empty body parses to nothing, and a document
 *  without a string `version` carries no id. Those are facts about this
 *  build, and the next ask would put the same question to the same build. A
 *  5xx, another status, or a read that fails is a moment: the next interval
 *  may succeed. */
export async function answerFor(
  response: VersionResponse,
): Promise<VersionAnswer> {
  if (response.status === 404 || response.status === 410) {
    return { kind: "absent" };
  }
  if (!response.ok) {
    throw new Error(`version probe answered ${response.status}`);
  }
  // A read that fails throws, and the watch treats that as a moment.
  const text = await response.text();
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    // The bytes arrived and they are not a version document: a fallback
    // page, an empty body, broken JSON.
    return { kind: "absent" };
  }
  if (
    typeof body === "object" &&
    body !== null &&
    "version" in body &&
    typeof body.version === "string"
  ) {
    return { kind: "id", candidate: body.version };
  }
  return { kind: "absent" };
}

/** The shell's ask: `<base>/_app/version.json?t=<now>`, read by `answerFor`.
 *  `?t=` keeps an edge that caches the file from hiding a deployment. */
export async function fetchVersionAnswer(
  base: string,
  fetchImpl?: VersionFetch,
  now: () => number = Date.now,
): Promise<VersionAnswer> {
  const doFetch = fetchImpl ?? (globalThis as { fetch?: VersionFetch }).fetch;
  if (doFetch === undefined) {
    throw new Error(
      "fetchVersionAnswer needs a fetch (call it in the browser)",
    );
  }
  return answerFor(await doFetch(`${base}/_app/version.json?t=${now()}`));
}

export interface VersionWatchOptions {
  /** The running bundle's id (`$app/environment`'s `version`). */
  readonly current: string;
  /** One ask: the deployed id to weigh, or `absent` when the deployment
   *  publishes none. A throw says the ask itself failed. The difference
   *  decides whether the watch asks again (see `check`). */
  readonly fetchVersion: () => Promise<VersionAnswer>;
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
      const answer = await fetchVersion();
      if (stopped) return;
      if (answer.kind === "absent") {
        // The deployment says it publishes no id: no later ask of this
        // page can get a different answer, so the watch ends here. A
        // deployment that gains the file is a new build, and the page
        // loaded from it is the one that hears about the next build.
        stop();
        return;
      }
      if (!usableVersionId(answer.candidate)) {
        // A document that answers with something other than an id has said
        // what it has to say: the same build asked again returns the same
        // bytes.
        stop();
        return;
      }
      if (answer.candidate === current) return;
      stop();
      onAvailable();
    } catch {
      // A failed ask -- a 5xx, another status, a dropped connection -- is a
      // moment, not a fact about the bundle.
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
