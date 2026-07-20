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
//     are /tagmata + /settings (chat is not available until the agora chat
//     data-plane ships, so / and /approvals redirect to /tagmata).
//
//   - "offline" -- no auth, no identity. `connected` reflects the daemon
//     session. Offline routes are / (chat), /approvals, /settings. /tagmata is
//     unavailable and redirects to /.
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
    pathname === "/login" || pathname === "/register" || pathname === "/connect"
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
      // Already set up -> straight to chat (one redirect, not via /connect).
      if (args.connected) return { kind: "redirect", url: "/" };
      // Not connected: the form is the right place.
      if (args.pathname === "/connect") return { kind: "render" };
      // /login,/register are the wrong door for an offline user.
      return { kind: "redirect", url: "/connect" };
    }
    // online
    if (
      (args.pathname === "/login" || args.pathname === "/register") &&
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
    if (args.pathname === "/tagmata") return { kind: "redirect", url: "/" };
    // /, /approvals, /settings: pages own their disconnected empty state.
    return { kind: "render" };
  }

  // online protected
  if (args.pathname === "/" || args.pathname === "/approvals") {
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
