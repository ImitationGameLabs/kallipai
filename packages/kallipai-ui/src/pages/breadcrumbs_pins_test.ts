import { assert } from "@std/assert";

// Source-read pins for the shell breadcrumb chrome. The trail lives in
// the shell: the
// route table (lib/shell/breadcrumbs.ts) owns the segments and the
// desktop shell's chrome bar renders them (DesktopShell since the
// split). A typecheck cannot see that a page still mounts its own
// trail, that the tagma segments point at the details hub, or that the
// bar carries its divider -- these pins guard exactly that, same
// rationale as chrome_pins_test.

const TRAIL_TABLE = new URL("../lib/shell/breadcrumbs.ts", import.meta.url);
const DESKTOP_SHELL = new URL(
  "../lib/shell/DesktopShell.svelte",
  import.meta.url,
);

// Every page migrated off per-page trails. The negative pin
// below keeps them mount-free: a trail comes from the table, never from
// a page-level <Breadcrumbs> mount.
const MIGRATED_PAGES = [
  "./TagmataPage.svelte",
  "./TagmaProfilePage.svelte",
  "./TagmaChatPage.svelte",
  "./manage/OnlineManagePage.svelte",
  "./RoomsPage.svelte",
  "./RoomConversationPage.svelte",
  "./RoomSettingsPage.svelte",
  "./ChannelChatPage.svelte",
  "./SettingsPage.svelte",
  "./account/AccountHubPage.svelte",
  "./UserProfilePage.svelte",
  "./manage/OnlineAgentDetailPage.svelte",
].map((p) => new URL(p, import.meta.url));

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the trail table routes every tagma segment through the details hub",
  { permissions: { read: [TRAIL_TABLE] } },
  () => {
    const src = source(TRAIL_TABLE);
    assert(
      src.includes("href: tagmaDetailsPath(id)"),
      "the tagma segment href must come from the details-hub builder (stable overview target)",
    );
    assert(
      !src.includes("tagmaChatPath"),
      "the chat path must not appear in the table (one target per label)",
    );
    assert(
      src.split("entry(").length - 1 === 14,
      "the table must cover the twelve migrated routes + the direct-session page + the files page",
    );
  },
);

Deno.test(
  "the desktop shell renders the single chrome bar with its divider",
  { permissions: { read: [DESKTOP_SHELL] } },
  () => {
    const src = source(DESKTOP_SHELL);
    assert(src.includes("{#if trail}"), "the bar must be table-gated");
    assert(
      src.includes("<Breadcrumbs segments={trail} />"),
      "the chrome bar must mount the trail",
    );
    assert(
      src.includes("border-b border-surface-200-800"),
      "the bar must carry the divider (one border everywhere)",
    );
    assert(
      src.includes("min-h-9"),
      "the bar must keep a constant single-row height",
    );
  },
);

Deno.test(
  "no migrated page mounts its own trail",
  { permissions: { read: MIGRATED_PAGES } },
  () => {
    for (const page of MIGRATED_PAGES) {
      const src = source(page);
      assert(
        !src.includes("<Breadcrumbs"),
        `${page} must not mount a trail (the shell chrome bar owns it)`,
      );
    }
  },
);

Deno.test(
  "the mobile back row keeps its route policy in the wired module",
  { permissions: { read: [TRAIL_TABLE] } },
  () => {
    const src = source(TRAIL_TABLE);
    // backFromTrail (the pure walk) lives in trailMatch.ts; the policy
    // -- /account and /tagmata keep the bar, conversations chain through
    // their chats-hub domain
    // -- lives with the wired table, next to the data it polices.
    assert(
      src.includes("backFromTrail(matchTrail(pathname))"),
      "the back row target must be trail-derived",
    );
    assert(
      src.includes('pathname === "/account"'),
      "the account hub must be excluded (a bar cell destination)",
    );
    assert(
      src.includes('href: "/chats"'),
      "conversations chain through their chats-hub domain in the table",
    );
  },
);
