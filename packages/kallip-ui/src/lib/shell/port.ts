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
