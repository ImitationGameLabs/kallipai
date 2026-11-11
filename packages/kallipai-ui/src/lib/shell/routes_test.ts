import { assert, assertEquals } from "@std/assert";
import {
  agentHistoryPath,
  adminGatewayCollectionPath,
  adminGatewayPath,
  filesPath,
  gatewayPath,
  tagmaAgentPath,
  tagmaChatPath,
  tagmaDetailsPath,
  tagmaDetailsSectionPath,
} from "./routes.ts";

Deno.test("tagmaChatPath builds the tagma-centric chat route", () => {
  assertEquals(tagmaChatPath("abc"), "/tagma/abc/chat");
});

Deno.test("tagmaDetailsPath builds the manage details hub", () => {
  assertEquals(tagmaDetailsPath("abc"), "/tagma/abc/details");
});

Deno.test("tagmaDetailsSectionPath builds the details sections", () => {
  assertEquals(
    tagmaDetailsSectionPath("abc", "overview"),
    "/tagma/abc/details/overview",
  );
  assertEquals(
    tagmaDetailsSectionPath("abc", "schedules"),
    "/tagma/abc/details/schedules",
  );
});

Deno.test("tagmaAgentPath builds the agent detail page route", () => {
  assertEquals(tagmaAgentPath("abc", "a1"), "/tagma/abc/details/agents/a1");
});

Deno.test(
  "agentHistoryPath hangs the turn history off either base path",
  () => {
    assertEquals(
      agentHistoryPath("/tagma/abc/details", "a1"),
      "/tagma/abc/details/agents/a1/history",
    );
    assertEquals(
      agentHistoryPath("/local/manage", "a1"),
      "/local/manage/agents/a1/history",
    );
  },
);

// The route list (and with it which page each path renders, its section,
// and the hub redirects) lives in routeTable.ts, one row per route, with
// the section rows built from one typed list. The path-builder tests below
// pin the URLs the table's redirects and props are built from.

Deno.test("filesPath builds the global files route", () => {
  assertEquals(filesPath(), "/files");
});

Deno.test("adminGatewayPath builds the admin gateway route", () => {
  assertEquals(adminGatewayPath(), "/admin/gateway");
});

Deno.test("adminGatewayCollectionPath builds the detail subface route", () => {
  assertEquals(
    adminGatewayCollectionPath("gpt"),
    "/admin/gateway/collections/gpt",
  );
  assertEquals(
    adminGatewayCollectionPath("a b"),
    "/admin/gateway/collections/a%20b",
  );
});

Deno.test("gatewayPath builds the user gateway route", () => {
  assertEquals(gatewayPath(), "/gateway");
});

const FILES_PAGE = new URL("../../pages/FilesPage.svelte", import.meta.url);
const ROOT_LAYOUT = new URL("./RootLayout.svelte", import.meta.url);

Deno.test(
  "/files rides the bar-destination shell-row form",
  // Shell parity: the mobileTitles row carries the small-screen
  // heading (a page h1 under the system bar strands it on edge-to-edge
  // PWAs), and the in-page h1 shows only from md+.
  { permissions: { read: [FILES_PAGE, ROOT_LAYOUT] } },
  () => {
    const page = new TextDecoder().decode(Deno.readFileSync(FILES_PAGE));
    assert(page.includes("hidden md:block"), "h1 yields to the shell row");
    assert(
      page.includes("px-2 py-4 md:p-6"),
      "container follows the small-screen padding",
    );
    const layout = new TextDecoder().decode(Deno.readFileSync(ROOT_LAYOUT));
    assert(
      layout.includes('\"/files\": nav_files'),
      "mobileTitles must carry the /files entry",
    );
  },
);

/** The direct shell still serves the turn history page its path builder
 * names. The online trees are served by the route table now, so this is the
 * only check that the builder's last segment exists in a real route tree: a
 * page that 404s while every type still checks looks exactly like this. */
const DIRECT_HISTORY_PAGE = new URL(
  "../../../../kallipai-direct/src/routes/local/manage/agents/[id]/history/+page.svelte",
  import.meta.url,
);

Deno.test(
  "the direct shell serves the turn history page the builder names",
  { permissions: { read: [DIRECT_HISTORY_PAGE] } },
  () => {
    const segment = agentHistoryPath("/local/manage", "a1").split("/").pop();
    assert(segment, "the builder must name a page");
    const dir = new URL(".", DIRECT_HISTORY_PAGE).pathname;
    assert(
      dir.endsWith(`/${segment}/`),
      `${dir} must be the route the builder names (${segment})`,
    );
    const src = new TextDecoder().decode(
      Deno.readFileSync(DIRECT_HISTORY_PAGE),
    );
    assert(src.includes("AgentHistoryPage"), "renders the shared page");
  },
);
