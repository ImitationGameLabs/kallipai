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
  assertEquals(isPublicRoute("/auth/signup"), true);
  assertEquals(isPublicRoute("/tagmata"), false);
  assertEquals(isPublicRoute("/"), false);
});

Deno.test("config not loaded -> skeleton on every route (incl. /login)", () => {
  for (
    const pathname of [
      "/",
      "/login",
      "/register",
      "/connect",
      "/tagmata",
      "/settings",
    ]
  ) {
    assertEquals(decide({ loaded: false, mode: "online", pathname }), {
      kind: "skeleton",
    });
  }
});

// --- offline public ---

Deno.test("offline + /connect + connected -> redirect /local/chat", () => {
  assertEquals(
    decide({ mode: "offline", pathname: "/connect", connected: true }),
    { kind: "redirect", url: "/local/chat" },
  );
});

Deno.test("offline + /connect + disconnected -> render the form", () => {
  assertEquals(
    decide({ mode: "offline", pathname: "/connect", connected: false }),
    { kind: "render" },
  );
});

Deno.test(
  "offline + /login + connected -> redirect /local/chat (one hop)",
  () => {
    assertEquals(
      decide({ mode: "offline", pathname: "/login", connected: true }),
      { kind: "redirect", url: "/local/chat" },
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
  "offline + /tagmata -> redirect /local/chat (no tagmata offline)",
  () => {
    assertEquals(decide({ mode: "offline", pathname: "/tagmata" }), {
      kind: "redirect",
      url: "/local/chat",
    });
  },
);

Deno.test(
  "offline + /rooms -> redirect /local/chat (rooms are online-only)",
  () => {
    assertEquals(decide({ mode: "offline", pathname: "/rooms" }), {
      kind: "redirect",
      url: "/local/chat",
    });
  },
);

Deno.test("offline + / -> redirect /local/chat (old offline root)", () => {
  assertEquals(decide({ mode: "offline", pathname: "/" }), {
    kind: "redirect",
    url: "/local/chat",
  });
});

Deno.test("offline + /chat/{non-local} -> redirect /local/chat", () => {
  assertEquals(decide({ mode: "offline", pathname: "/chat/abc" }), {
    kind: "redirect",
    url: "/local/chat",
  });
});

Deno.test("offline + /chat/local (old path) -> redirect /local/chat (back-compat)", () => {
  assertEquals(decide({ mode: "offline", pathname: "/chat/local" }), {
    kind: "redirect",
    url: "/local/chat",
  });
});

Deno.test(
  "offline + /rooms/{id} -> redirect /local/chat (rooms are online-only)",
  () => {
    assertEquals(decide({ mode: "offline", pathname: "/rooms/room-1" }), {
      kind: "redirect",
      url: "/local/chat",
    });
  },
);

Deno.test("offline protected routes render (the local chat + settings)", () => {
  for (const pathname of ["/local/chat", "/local/manage/overview", "/local/manage/budget", "/settings"]) {
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

Deno.test("online + /rooms + signed-in -> render", () => {
  // /rooms is a new online-protected route: it falls through (no collapse rule
  // for it) to the user checks then render. Pin the fall-through so a future
  // gate refactor that adds an online allow-list does not silently break it.
  assertEquals(decide({ mode: "online", pathname: "/rooms", user: USER }), {
    kind: "render",
  });
});

Deno.test("online + /rooms + logged-out -> redirect /login", () => {
  assertEquals(decide({ mode: "online", pathname: "/rooms", user: null }), {
    kind: "redirect",
    url: "/login?next=" + encodeURIComponent("/rooms"),
  });
});

Deno.test(
  "online + /local/chat -> redirect /tagmata (offline route marker)",
  () => {
    // /local/chat is an offline-only route; it is never a valid online
    // destination. Mirrors the offline branch collapsing /tagmata -> /local/chat.
    assertEquals(
      decide({ mode: "online", pathname: "/local/chat", user: USER }),
      { kind: "redirect", url: "/tagmata" },
    );
    // Fires during the whoami-in-flight window too, so the URL is corrected
    // before the user resolves (no stuck "Connecting..." on ChannelChatPage).
    assertEquals(
      decide({ mode: "online", pathname: "/local/chat", user: undefined }),
      { kind: "redirect", url: "/tagmata" },
    );
    // The rule sits above the user checks, so a logged-out user still collapses
    // to /tagmata (whose next pass sends to /login) rather than /login?next=
    // /local/chat -- locks the ordering the source comment relies on.
    assertEquals(
      decide({ mode: "online", pathname: "/local/chat", user: null }),
      { kind: "redirect", url: "/tagmata" },
    );
  },
);

Deno.test("online + /local/manage/* -> redirect /tagmata (offline-only)", () => {
  assertEquals(
    decide({ mode: "online", pathname: "/local/manage/overview", user: USER }),
    { kind: "redirect", url: "/tagmata" },
  );
});

Deno.test("online + /chat/local (old path) -> redirect /tagmata (back-compat)", () => {
  assertEquals(
    decide({ mode: "online", pathname: "/chat/local", user: USER }),
    { kind: "redirect", url: "/tagmata" },
  );
});

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

Deno.test("online + /auth/signup + signed-in -> redirect /tagmata", () => {
  // A signed-in user has no business on the OAuth signup step; mirror /register.
  assertEquals(
    decide({ mode: "online", pathname: "/auth/signup", user: USER }),
    { kind: "redirect", url: "/tagmata" },
  );
});

Deno.test("online + /auth/signup + logged-out -> render", () => {
  assertEquals(
    decide({ mode: "online", pathname: "/auth/signup", user: null }),
    { kind: "render" },
  );
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
