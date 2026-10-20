// Source-read pins for the mention chips (same rationale as
// message_bubble_test: the wiring a typecheck cannot see). Only the four things
// that would ship a wrong chip are held:
//   - the chip row renders after the body and before the failure copy,
//   - a chip re-uses <SenderIdentity> rather than drawing its own identity
//     tokens (a second home for the handle vocabulary is the regression),
//   - the two unnamed states stay apart: a landed roster that lacks the id draws
//     the unknown-member copy, an unlanded roster draws the bare short id,
//   - the viewer's own chip carries the self word.
// Class strings are deliberately NOT pinned (a restyle is not a defect), and
// the catalog guard already holds key parity, placeholders, and one usage per
// key -- so no per-key assertions here.

import { assert } from "@std/assert";

const BUBBLE = new URL("./MessageBubble.svelte", import.meta.url);
const CHIPS = new URL("./MentionChips.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

// The text a Svelte branch renders: from its opener to the next branch or
// closing tag. A copy pinned here has to sit inside that branch.
function branch(src: string, opener: string): string {
  const at = src.indexOf(opener);
  if (at < 0) return "";
  const rest = src.slice(at + opener.length);
  const end = rest.search(/[{][:/]/u);
  return end < 0 ? rest : rest.slice(0, end);
}

Deno.test(
  "the chip row renders after the body and before the failure copy",
  { permissions: { read: [BUBBLE] } },
  () => {
    const src = source(BUBBLE);
    const guard = src.indexOf("{#if mentions && mentions.length > 0}");
    const chips = src.indexOf("<MentionChips chips={mentions} />");
    const failure = src.indexOf("{#if failureCopy}");
    assert(guard >= 0, "the chip row sits under its own guard");
    assert(chips > guard, "the chips render inside that guard");
    assert(failure > chips, "the failure copy stays the last thing in the box");
  },
);

Deno.test(
  "a chip re-uses SenderIdentity instead of its own identity tokens",
  { permissions: { read: [CHIPS] } },
  () => {
    const src = source(CHIPS);
    assert(src.includes("<SenderIdentity"), "the chip draws a sender identity");
    assert(
      !src.includes("@lucide/svelte"),
      "the chip must not draw its own kind icon",
    );
    assert(
      !src.includes("parseParticipantHandle"),
      "the chip must not re-split the handle",
    );
  },
);

Deno.test(
  "each unnamed state keeps its own rendering, and the viewer keeps its word",
  { permissions: { read: [CHIPS] } },
  () => {
    const src = source(CHIPS);
    assert(
      src.includes('chip.unnamed === "unknown"'),
      "the chip tells the two unnamed states apart",
    );
    assert(
      branch(src, '{:else if chip.unnamed === "unknown"}').includes(
        "chat_mention_unknown(",
      ),
      "a landed roster without the id says so",
    );
    assert(
      branch(src, "{:else}").includes("·{shortIdOf(chip.id)}"),
      "an unlanded roster draws the bare short id and claims nothing",
    );
    assert(src.includes("chat_mention_self()"));
    assert(
      src.includes("chat_mentions_aria()"),
      "the list keeps its group name",
    );
  },
);

Deno.test(
  "the chips only grow a remove button when the caller wires one",
  () => {
    // The composer's picked strip removes a member; a message's mentions do
    // not (they report what the sender named, and nothing rewrites that), so
    // the button must be reachable only through the optional prop.
    const src = source(CHIPS);
    assert(
      src.includes("onRemove?: (id: string) => void;"),
      "the prop is optional",
    );
    assert(
      branch(src, "{#if onRemove}").includes(
        "onclick={() => onRemove(chip.id)}",
      ),
      "absent, the capsule is a plain report",
    );
    assert(
      branch(src, "{#if onRemove}").includes("chat_mention_remove_aria("),
      "the button says what it removes",
    );
  },
);
