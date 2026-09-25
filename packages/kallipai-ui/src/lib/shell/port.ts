// ShellPort: the only navigation primitive this package consumes. `$app/navigation`
// is a SvelteKit virtual module that does NOT resolve inside a library package
// (it's surfaced via the consuming app's generated tsconfig), so the app injects
// its real `goto` at bootstrap. Route location (pathname/search) is passed as
// props to <RootLayout> by the app, which reads `$app/state` -- props stay
// reactive without this package touching `$app/*`.

export interface GotoOptions {
  replaceState?: boolean;
}

export type Goto = (url: string, opts?: GotoOptions) => Promise<void>;

let goto: Goto | null = null;

/** Inject the SvelteKit goto. Called once at app bootstrap. */
export function initShell(g: Goto): void {
  goto = g;
}

export function navigate(url: string, opts?: GotoOptions): Promise<void> {
  if (!goto) throw new Error("initShell(goto) must be called at app bootstrap");
  return goto(url, opts);
}

// The shell's product mode, from the shell identity flag rather than the
// persisted config: since the offline world moved to the kallipai-direct
// package, a web/app shell is always "online" (a stored offline config
// from an older build is clamped away -- its routes and entries no longer
// exist there), and the direct shell is always "offline" (archeion is
// unreachable). Components read this instead of re-deriving the mode
// from the persisted config, so the mode can never disagree between the
// layout and the chrome.
let offlineOnlyShell = false;

/** Declare the host shell offline-only (kallipai-direct). Called once at bootstrap. */
export function setOfflineOnlyShell(): void {
  offlineOnlyShell = true;
}

export function isOfflineOnlyShell(): boolean {
  return offlineOnlyShell;
}

/** The mode this shell can ever be in; see the comment above. */
export function shellMode(): "online" | "offline" {
  return offlineOnlyShell ? "offline" : "online";
}
