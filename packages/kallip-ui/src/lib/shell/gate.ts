// Mode + auth gate decision logic, extracted for unit testing. The
// <RootLayout> component calls appGateDecision() inside a $effect and acts on
// the result.
//
// Two modes (derived from the persisted config's activeMode via modeOf); only
// one is active at a time, though both sessions may be retained underneath:
//
//   - "online" -- agora passkey auth. `user` is the tri-state from
//     AgoraSessionStore: `undefined` = unresolved (whoami running / failed),
//     `null` = resolved logged-out, object = signed in. `authError` is set when
//     whoami failed with a non-auth error (e.g. agora unreachable). Online routes
//     are /tagmata + /settings + /chat/{server-id} (relay conversations). `/`,
//     /local/* (offline-only routes), and the retired `/chat/local` marker are
//     not valid online destinations, so all redirect to /tagmata.
//
//   - "offline" -- no auth, no identity. `connected` reflects the local tagma
//     transport. Offline routes are /local/* (chat + management). /tagmata
//     is unavailable and redirects to /local; `/` redirects to /local.
//
// Public (front-door) routes are /login, /register (online) and /connect
// (offline). The gate owns all post-mode-flip / post-connect navigation: pages
// must not navigate after a config write or connect.
//
// Both sessions may coexist: the persisted config retains offline creds and the
// agora cookie survives across switches (neither side is destroyed on a mode
// flip), so switching is re-auth-free in both directions. Switching is an
// explicit user action (Settings handlers / Connect submit); /connect is
// reachable by anyone -- a signed-in online user browsing the offline setup
// form has not switched modes yet.
//
// `loaded` gates everything: until the persisted config has loaded we cannot
// know the mode, so every route shows the skeleton (no flash of the wrong
// front-door). whoami runs once at boot (online only), so an unresolved user
// past the brief resolving window means the agora is down -- in that case we
// route to /login (which surfaces the error in context) rather than trapping
// the user on a blank skeleton.

import type { AppMode } from "../config/mode.ts";

export type GateDecision =
  | { kind: "render" }
  | { kind: "skeleton" }
  | { kind: "redirect"; url: string };

export function isPublicRoute(pathname: string): boolean {
  return (
    pathname === "/login" ||
    pathname === "/register" ||
    pathname === "/pair" ||
    pathname === "/connect" ||
    pathname === "/auth/callback" ||
    pathname === "/auth/signup"
  );
}

export function appGateDecision(args: {
  loaded: boolean;
  mode: AppMode;
  user: unknown;
  authError: string | null;
  connected: boolean;
  pathname: string;
  search: string;
}): GateDecision {
  // Config still loading -> mode unknown -> skeleton on every route.
  if (!args.loaded) return { kind: "skeleton" };

  const pub = isPublicRoute(args.pathname);

  if (pub) {
    if (args.mode === "offline") {
      // Already set up -> straight to the local home (one redirect, not via
      // /connect).
      if (args.connected) return { kind: "redirect", url: "/local" };
      // Not connected: the form is the right place.
      if (args.pathname === "/connect") return { kind: "render" };
      // /login,/register are the wrong door for an offline user.
      return { kind: "redirect", url: "/connect" };
    }
    // online
    // A signed-in user hitting /pair (the anonymous-device code-entry page)
    // goes to settings, where the mint UI lives — landing them on the code
    // entry would be wrong (and would mint a second session for an already-
    // authed user).
    if (
      args.pathname === "/pair" &&
      args.user != null &&
      args.user !== undefined
    ) {
      return { kind: "redirect", url: "/settings" };
    }
    if (
      (args.pathname === "/login" ||
        args.pathname === "/register" ||
        args.pathname === "/auth/signup") &&
      args.user != null &&
      args.user !== undefined
    ) {
      return { kind: "redirect", url: "/tagmata" };
    }
    // /connect (the offline entry) renders for everyone -- signed-in or not.
    // Unsigned /login, /register render.
    return { kind: "render" };
  }

  // Protected routes.
  if (args.mode === "offline") {
    // All /local/* routes render in offline mode (the single local-only gate
    // covering chat + management).
    if (args.pathname === "/local" || args.pathname.startsWith("/local/")) {
      return { kind: "render" };
    }
    // Back-compat: old /chat/local → /local.
    if (args.pathname === "/chat/local") {
      return { kind: "redirect", url: "/local" };
    }
    // /tagmata + /rooms are online-only (the agora control plane is
    // unreachable offline); `/` is the old offline root. A non-local
    // /chat/{id} deep link is meaningless offline (no relay conversations
    // exist). All collapse to the local home.
    if (
      args.pathname === "/tagmata" ||
      args.pathname === "/rooms" ||
      args.pathname === "/" ||
      args.pathname.startsWith("/chat/") ||
      args.pathname.startsWith("/rooms/")
    ) {
      return { kind: "redirect", url: "/local" };
    }
    // /settings: page owns its disconnected empty state.
    return { kind: "render" };
  }

  // online protected
  // `/` is the old root; `/chat/local` is a retired offline-only route
  // marker; `/local/*` is the offline-only route tree (chat + management).
  // None are valid online destinations, so go to the online home. Placed
  // above the user checks so it also fires during the whoami-in-flight
  // window; the next iteration on /tagmata then resolves auth.
  if (
    args.pathname === "/" ||
    args.pathname === "/local" ||
    args.pathname.startsWith("/local/") ||
    args.pathname === "/chat/local"
  ) {
    return { kind: "redirect", url: "/tagmata" };
  }
  if (args.user === null) {
    const next = args.pathname + args.search;
    return { kind: "redirect", url: `/login?next=${encodeURIComponent(next)}` };
  }
  if (args.user === undefined && args.authError) {
    return { kind: "redirect", url: "/login" };
  }
  if (args.user === undefined) {
    return { kind: "skeleton" };
  }
  return { kind: "render" };
}
