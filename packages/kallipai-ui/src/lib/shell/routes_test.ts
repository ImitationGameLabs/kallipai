import { assert, assertEquals } from "@std/assert";
import {
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

const DETAILS_PAGES = [
  "overview",
  "budget",
  "agents",
  "profiles",
  "schedules",
].map((section) => ({
  section,
  pages: [
    new URL(
      `../../../../kallipai-web/src/routes/tagma/[id]/details/${section}/+page.svelte`,
      import.meta.url,
    ),
    new URL(
      `../../../../kallipai-app/src/routes/tagma/[id]/details/${section}/+page.svelte`,
      import.meta.url,
    ),
  ],
}));

Deno.test(
  "the details section pages render their own section",
  // Same rot-risk one tree over: a thin section page could copy-paste a
  // neighbor's page constant and still typecheck. Read all ten shells and
  // assert the section each one renders. Scoped read grant, same rationale.
  {
    permissions: {
      read: DETAILS_PAGES.flatMap(({ pages }) => [...pages]),
    },
  },
  () => {
    for (const { section, pages } of DETAILS_PAGES) {
      for (const page of pages) {
        const src = new TextDecoder().decode(Deno.readFileSync(page));
        assert(
          src.includes(`page="${section}"`),
          "section page must render its own section",
        );
        assert(
          src.includes("page.params.id"),
          "section page must forward the route param, not a literal",
        );
      }
    }
  },
);

const DETAILS_HUBS = [
  new URL(
    "../../../../kallipai-web/src/routes/tagma/[id]/details/+page.ts",
    import.meta.url,
  ),
  new URL(
    "../../../../kallipai-app/src/routes/tagma/[id]/details/+page.ts",
    import.meta.url,
  ),
];

Deno.test(
  "the details hub stays wired to the path builder",
  // The hub 301s to overview through the builder; pin the wiring so a
  // downgrade to 302 or a re-derived literal cannot slip in. Scoped read
  // grant, same rationale.
  { permissions: { read: DETAILS_HUBS } },
  () => {
    for (const hub of DETAILS_HUBS) {
      const src = new TextDecoder().decode(Deno.readFileSync(hub));
      assert(src.includes("redirect(301"), "hub must issue a permanent 301");
      assert(
        src.includes("tagmaDetailsSectionPath(params.id"),
        "hub must forward via the path builder, not a re-derived literal",
      );
      assert(
        src.includes('"overview"'),
        "hub must land on the overview section",
      );
    }
  },
);

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

const FILES_SHELLS = [
  new URL(
    "../../../../kallipai-app/src/routes/files/+page.svelte",
    import.meta.url,
  ),
  new URL(
    "../../../../kallipai-web/src/routes/files/+page.svelte",
    import.meta.url,
  ),
];

Deno.test(
  "the /files shells stay thin and render the shared FilesPage",
  // Same rot-risk one tree over: a shell could re-derive the page inline
  // or drift between hosts. Scoped read grant, same rationale.
  { permissions: { read: FILES_SHELLS } },
  () => {
    for (const shell of FILES_SHELLS) {
      const src = new TextDecoder().decode(Deno.readFileSync(shell));
      assert(src.includes("FilesPage"), "shell must render FilesPage");
      assert(
        src.includes("@kallipai/kallipai-ui"),
        "shell must import the shared package, not a local copy",
      );
    }
  },
);

const ADMIN_HUBS = [
  new URL(
    "../../../../kallipai-web/src/routes/admin/+page.ts",
    import.meta.url,
  ),
  new URL(
    "../../../../kallipai-app/src/routes/admin/+page.ts",
    import.meta.url,
  ),
];

Deno.test(
  "the admin hub stays wired to the path builder",
  // The hub 301s to the gateway console through the builder; pin the
  // wiring so a downgrade to 302 or a re-derived literal cannot slip
  // in. Scoped read grant, same rationale as the details hubs.
  { permissions: { read: ADMIN_HUBS } },
  () => {
    for (const hub of ADMIN_HUBS) {
      const src = new TextDecoder().decode(Deno.readFileSync(hub));
      assert(src.includes("redirect(301"), "hub must issue a permanent 301");
      assert(
        src.includes("adminGatewayPath()"),
        "hub must forward via the path builder, not a re-derived literal",
      );
    }
  },
);
