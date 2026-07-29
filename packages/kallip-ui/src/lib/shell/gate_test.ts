import { assertEquals } from "@std/assert";
import { appGateDecision, isPublicRoute } from "./gate.ts";
import type { AppMode } from "../config/mode.ts";

const USER = { username: "alice" };

function decide(
  over: Partial<Parameters<typeof appGateDecision>[0]> & {
    mode: AppMode;
    pathname: string;
  },
) {
  return appGateDecision({
    loaded: true,
    user: undefined,
    authError: null,
    connected: false,
    search: "",
    ...over,
  });
}

Deno.test("isPublicRoute flags /login, /register, /connect", () => {
  assertEquals(isPublicRoute("/login"), true);
  assertEquals(isPublicRoute("/register"), true);
  assertEquals(isPublicRoute("/connect"), true);
  assertEquals(isPublicRoute("/tagmata"), false);
  assertEquals(isPublicRoute("/"), false);
});

Deno.test("config not loaded -> skeleton on every route (incl. /login)", () => {
  for (const pathname of [
    "/",
    "/login",
    "/register",
    "/connect",
    "/tagmata",
    "/settings",
  ]) {
    assertEquals(decide({ loaded: false, mode: "online", pathname }), {
      kind: "skeleton",
    });
  }
});

// --- offline public ---

Deno.test("offline + /connect + connected -> redirect /chat/local", () => {
  assertEquals(
    decide({ mode: "offline", pathname: "/connect", connected: true }),
    { kind: "redirect", url: "/chat/local" },
  );
});

Deno.test("offline + /connect + disconnected -> render the form", () => {
  assertEquals(
    decide({ mode: "offline", pathname: "/connect", connected: false }),
    { kind: "render" },
  );
});

Deno.test(
  "offline + /login + connected -> redirect /chat/local (one hop)",
  () => {
    assertEquals(
      decide({ mode: "offline", pathname: "/login", connected: true }),
      { kind: "redirect", url: "/chat/local" },
    );
  },
);

Deno.test("offline + /login + disconnected -> redirect /connect", () => {
  assertEquals(
    decide({ mode: "offline", pathname: "/login", connected: false }),
    { kind: "redirect", url: "/connect" },
  );
});

// --- offline protected ---

Deno.test(
  "offline + /tagmata -> redirect /chat/local (no tagmata offline)",
  () => {
    assertEquals(decide({ mode: "offline", pathname: "/tagmata" }), {
      kind: "redirect",
      url: "/chat/local",
    });
  },
);

Deno.test("offline + / -> redirect /chat/local (old offline root)", () => {
  assertEquals(decide({ mode: "offline", pathname: "/" }), {
    kind: "redirect",
    url: "/chat/local",
  });
});

Deno.test("offline + /chat/{non-local} -> redirect /chat/local", () => {
  assertEquals(decide({ mode: "offline", pathname: "/chat/abc" }), {
    kind: "redirect",
    url: "/chat/local",
  });
});

Deno.test("offline protected routes render (the local chat + settings)", () => {
  for (const pathname of ["/chat/local", "/settings"]) {
    assertEquals(decide({ mode: "offline", pathname }), { kind: "render" });
  }
});

// --- online public ---

Deno.test(
  "online + /connect renders for everyone (offline entry; mutual exclusivity is enforced at the transition, not the gate)",
  () => {
    assertEquals(
      decide({ mode: "online", pathname: "/connect", user: undefined }),
      { kind: "render" },
    );
    assertEquals(decide({ mode: "online", pathname: "/connect", user: USER }), {
      kind: "render",
    });
  },
);

Deno.test("online + /chat/{id} renders for a signed-in user", () => {
  // A protected, non-/ route falls through to render once the user is resolved;
  // the gate does not enumerate every channel id.
  assertEquals(
    decide({ mode: "online", pathname: "/chat/conv-1", user: USER }),
    { kind: "render" },
  );
});

Deno.test(
  "online + /chat/local -> redirect /tagmata (offline route marker)",
  () => {
    // /chat/local is an offline-only route; it is never a valid online
    // destination. Mirrors the offline branch collapsing /tagmata -> /chat/local.
    assertEquals(
      decide({ mode: "online", pathname: "/chat/local", user: USER }),
      { kind: "redirect", url: "/tagmata" },
    );
    // Fires during the whoami-in-flight window too, so the URL is corrected
    // before the user resolves (no stuck "Connecting..." on ChannelChatPage).
    assertEquals(
      decide({ mode: "online", pathname: "/chat/local", user: undefined }),
      { kind: "redirect", url: "/tagmata" },
    );
    // The rule sits above the user checks, so a logged-out user still collapses
    // to /tagmata (whose next pass sends to /login) rather than /login?next=
    // /chat/local -- locks the ordering the source comment relies on.
    assertEquals(
      decide({ mode: "online", pathname: "/chat/local", user: null }),
      { kind: "redirect", url: "/tagmata" },
    );
  },
);

Deno.test("online + /chat/{id} + logged-out -> redirect /login", () => {
  assertEquals(
    decide({ mode: "online", pathname: "/chat/conv-1", user: null }),
    {
      kind: "redirect",
      url: "/login?next=" + encodeURIComponent("/chat/conv-1"),
    },
  );
});

Deno.test("online + /login + signed-in -> redirect /tagmata", () => {
  assertEquals(decide({ mode: "online", pathname: "/login", user: USER }), {
    kind: "redirect",
    url: "/tagmata",
  });
});

Deno.test("online + /login + unresolved -> render (no flash)", () => {
  assertEquals(
    decide({ mode: "online", pathname: "/login", user: undefined }),
    {
      kind: "render",
    },
  );
});

Deno.test("online + /login + logged-out -> render", () => {
  assertEquals(decide({ mode: "online", pathname: "/login", user: null }), {
    kind: "render",
  });
});

// --- online protected ---

Deno.test(
  "online + / -> redirect /tagmata (chat not in online mode yet)",
  () => {
    assertEquals(decide({ mode: "online", pathname: "/", user: USER }), {
      kind: "redirect",
      url: "/tagmata",
    });
  },
);

Deno.test("online protected + logged-out -> /login?next=...", () => {
  assertEquals(
    decide({
      mode: "online",
      pathname: "/tagmata",
      user: null,
      search: "?x=1",
    }),
    { kind: "redirect", url: "/login?next=%2Ftagmata%3Fx%3D1" },
  );
});

Deno.test("online protected + agora unreachable -> /login (no next)", () => {
  assertEquals(
    decide({
      mode: "online",
      pathname: "/tagmata",
      user: undefined,
      authError: "fetch failed",
    }),
    { kind: "redirect", url: "/login" },
  );
});

Deno.test("online protected + resolving -> skeleton", () => {
  assertEquals(
    decide({ mode: "online", pathname: "/tagmata", user: undefined }),
    { kind: "skeleton" },
  );
});

Deno.test("online protected + signed-in -> render", () => {
  assertEquals(decide({ mode: "online", pathname: "/settings", user: USER }), {
    kind: "render",
  });
});
