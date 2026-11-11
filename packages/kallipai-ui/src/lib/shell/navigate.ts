/**
 * The navigation shape the shared views consume. A host passes its
 * `$app/navigation` `beforeNavigate` (structurally compatible) so nothing in
 * this package imports `$app/*`.
 */
export interface NavigationTarget {
  url?: { pathname: string } | null;
}

export interface NavigationAttempt {
  cancel: () => void;
  to?: NavigationTarget | null;
}

export type BeforeNavigateHook = (fn: (nav: NavigationAttempt) => void) => void;
