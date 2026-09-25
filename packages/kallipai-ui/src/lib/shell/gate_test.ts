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
    appKind: "app",
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

Deno.test("offline + /connect + connected (app) -> redirect /local", () => {
  assertEquals(
    decide({
      mode: "offline",
      pathname: "/connect",
      connected: true,
      appKind: "app",
    }),
    { kind: "redirect", url: "/local" },
  );
});

Deno.test("offline + /connect + disconnected -> render the form", () => {
  assertEquals(
    decide({ mode: "offline", pathname: "/connect", connected: false }),
    { kind: "render" },
  );
});

Deno.test(
  "offline + /login + connected (app) -> redirect /local (one hop)",
  () => {
    assertEquals(
      decide({
        mode: "offline",
        pathname: "/login",
        connected: true,
        appKind: "app",
      }),
      { kind: "redirect", url: "/local" },
    );
  },
);

Deno.test("offline + /login + disconnected -> redirect /connect", () => {
  assertEquals(
    decide({ mode: "offline", pathname: "/login", connected: false }),
    { kind: "redirect", url: "/connect" },
  );
});

Deno.test(
  "offline + /connect + connected (web) -> redirect /local/chat",
  () => {
    assertEquals(
      decide({
        mode: "offline",
        pathname: "/connect",
        connected: true,
        appKind: "web",
      }),
      { kind: "redirect", url: "/local/chat" },
    );
  },
);

Deno.test(
  "offline + /login + connected (web) -> redirect /local/chat (one hop)",
  () => {
    assertEquals(
      decide({
        mode: "offline",
        pathname: "/login",
        connected: true,
        appKind: "web",
      }),
      { kind: "redirect", url: "/local/chat" },
    );
  },
);

// --- offline protected ---

Deno.test("offline + /tagmata -> render (mode-neutral unified page)", () => {
  assertEquals(decide({ mode: "offline", pathname: "/tagmata" }), {
    kind: "render",
  });
});

Deno.test("offline + /rooms -> redirect /local (rooms are online-only)", () => {
  assertEquals(decide({ mode: "offline", pathname: "/rooms" }), {
    kind: "redirect",
    url: "/local",
  });
});

Deno.test("offline + / -> redirect /local (old offline root)", () => {
  assertEquals(decide({ mode: "offline", pathname: "/" }), {
    kind: "redirect",
    url: "/local",
  });
});

Deno.test("offline + /chat/{non-local} -> redirect /local", () => {
  assertEquals(decide({ mode: "offline", pathname: "/chat/abc" }), {
    kind: "redirect",
    url: "/local",
  });
});

Deno.test(
  "offline + /chat/local (old path) -> redirect /local (back-compat)",
  () => {
    assertEquals(decide({ mode: "offline", pathname: "/chat/local" }), {
      kind: "redirect",
      url: "/local",
    });
  },
);

Deno.test(
  "offline + /rooms/{id} -> redirect /local (rooms are online-only)",
  () => {
    assertEquals(decide({ mode: "offline", pathname: "/rooms/room-1" }), {
      kind: "redirect",
      url: "/local",
    });
  },
);

Deno.test(
  "offline protected routes render (local chat + settings + /tagmata)",
  () => {
    for (const pathname of [
      "/local/chat",
      "/local/manage/overview",
      "/local/manage/budget",
      "/settings",
      "/tagmata",
    ]) {
      assertEquals(decide({ mode: "offline", pathname }), { kind: "render" });
    }
  },
);

Deno.test(
  "/account renders in both modes (the account hub is mode-agnostic)",
  () => {
    // The hub page serves account actions in either mode, so the gate must
    // never fold /account into the /local/* offline-only tree (or the
    // online-only redirects).
    assertEquals(decide({ mode: "offline", pathname: "/account" }), {
      kind: "render",
    });
    assertEquals(decide({ mode: "online", pathname: "/account", user: USER }), {
      kind: "render",
    });
  },
);

Deno.test("offline + /chats and /chats/* -> redirect /local", () => {
  // The hub is online-only; offline deep links collapse to the local home
  // (the same scope family as /rooms).
  assertEquals(decide({ mode: "offline", pathname: "/chats" }), {
    kind: "redirect",
    url: "/local",
  });
  assertEquals(decide({ mode: "offline", pathname: "/chats/conv-1" }), {
    kind: "redirect",
    url: "/local",
  });
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
  "online + /local/chat -> redirect /chats (offline route marker)",
  () => {
    // /local/chat is an offline-only route; it is never a valid online
    // destination. Mirrors the offline branch collapsing /chats -> /local.
    assertEquals(
      decide({ mode: "online", pathname: "/local/chat", user: USER }),
      { kind: "redirect", url: "/chats" },
    );
    // Fires during the whoami-in-flight window too, so the URL is corrected
    // before the user resolves (no stuck "Connecting..." on ChannelChatPage).
    assertEquals(
      decide({ mode: "online", pathname: "/local/chat", user: undefined }),
      { kind: "redirect", url: "/chats" },
    );
    // The rule sits above the user checks, so a logged-out user still collapses
    // to /chats (whose next pass sends to /login) rather than /login?next=
    // /local/chat -- locks the ordering the source comment relies on.
    assertEquals(
      decide({ mode: "online", pathname: "/local/chat", user: null }),
      { kind: "redirect", url: "/chats" },
    );
  },
);

Deno.test("online + /local/manage/* -> redirect /chats (offline-only)", () => {
  assertEquals(
    decide({
      mode: "online",
      pathname: "/local/manage/overview",
      user: USER,
    }),
    { kind: "redirect", url: "/chats" },
  );
});

Deno.test(
  "online + /chat/local (old path) -> redirect /chats (back-compat)",
  () => {
    assertEquals(
      decide({ mode: "online", pathname: "/chat/local", user: USER }),
      { kind: "redirect", url: "/chats" },
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

Deno.test("online + /login + signed-in -> redirect / (home front door)", () => {
  assertEquals(decide({ mode: "online", pathname: "/login", user: USER }), {
    kind: "redirect",
    url: "/",
  });
});

Deno.test(
  "online + /auth/signup + signed-in -> redirect / (home front door)",
  () => {
    // A signed-in user has no business on the OAuth signup step; mirror /register.
    assertEquals(
      decide({ mode: "online", pathname: "/auth/signup", user: USER }),
      { kind: "redirect", url: "/" },
    );
  },
);

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

Deno.test("online + / -> render (the panorama home)", () => {
  assertEquals(decide({ mode: "online", pathname: "/", user: USER }), {
    kind: "render",
  });
});

Deno.test("online + / + resolving -> skeleton", () => {
  assertEquals(decide({ mode: "online", pathname: "/", user: undefined }), {
    kind: "skeleton",
  });
});

Deno.test("online + / + logged-out -> /login?next=%2F", () => {
  assertEquals(decide({ mode: "online", pathname: "/", user: null }), {
    kind: "redirect",
    url: "/login?next=%2F",
  });
});

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

Deno.test("online protected + archeion unreachable -> /login (no next)", () => {
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
