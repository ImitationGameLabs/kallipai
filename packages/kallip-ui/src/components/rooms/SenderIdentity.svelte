<script lang="ts">
  // A room participant's identity rendered as separate tokens: a kind icon
  // (Cpu = agent, User = human), the mutable display `label` (roster-resolved;
  // absent for unresolved senders), the stable `@handle` (the owning user's
  // username), and -- for agents only -- the unforgeable short participant-id
  // as a muted postfix. The tokens are decomposed from the relay-stamped
  // handle by `parseParticipantHandle`.
  //
  // Sizing/color contract: this renders BARE inline elements that inherit
  // font-size and color from the parent -- the chat bubble header wraps it in
  // `text-xs`, the member row in `text-sm`. It owns the relative dimming of its
  // tokens (icon size + the `@handle`/short-id opacities; the label stays at
  // full parent strength) plus an optional `class`. Callers must NOT wrap it in
  // their own `opacity-*`: CSS opacity multiplies through nesting, which would
  // compound with these child opacities and wash the handle/short-id out. Use
  // font-size, not opacity, to de-emphasize the whole header.
  //
  // When `href` is set the whole token group becomes a link to that sender's
  // profile. The link uses `text-inherit` so it keeps the parent color (an
  // unstyled `<a>` would adopt link color and clash with the token opacities),
  // carries a concise `aria-label`, and hides the inner tokens from AT so the
  // nested `role="img"` does not bloat the link's accessible name.
  //
  // The kind icon carries the human/agent distinction (previously a text
  // badge), so its wrapping span is `role="img"` with an `aria-label`; the
  // lucide glyph itself stays decorative (`aria-hidden`). Self-agnostic -- the
  // self distinction (bubble alignment+fill, member-row highlight) is applied
  // by the consumer.
  import { Cpu, User } from "@lucide/svelte";
  import { parseParticipantHandle } from "../../lib/room-message.ts";
  import {
    sender_agent_aria,
    sender_user_aria,
    sender_view_profile_aria,
  } from "../../paraglide/messages.js";

  let {
    kind,
    handle,
    label,
    href,
    class: klass = "",
  }: {
    kind: "human" | "agent";
    handle: string;
    label?: string;
    /** When set, the identity links to this URL (a sender profile). Absent ->
     *  a plain non-interactive token group. */
    href?: string;
    class?: string;
  } = $props();

  const parts = $derived(parseParticipantHandle(handle, kind));
</script>

{#snippet tokens()}
  <span
    class="shrink-0 inline-flex"
    role="img"
    aria-label={kind === "agent" ? sender_agent_aria() : sender_user_aria()}
  >
    {#if kind === "agent"}
      <Cpu class="size-3.5" aria-hidden="true" />
    {:else}
      <User class="size-3.5" aria-hidden="true" />
    {/if}
  </span>
  {#if label}<span class="truncate">{label}</span>{/if}
  <span class="opacity-60 truncate">{parts.handle}</span>
  {#if parts.shortId}
    <span class="font-mono opacity-50 shrink-0">·{parts.shortId}</span>
  {/if}
{/snippet}

{#if href}
  <a
    {href}
    aria-label={sender_view_profile_aria({ handle: parts.handle })}
    class="inline-flex items-center gap-1 min-w-0 rounded-base text-inherit hover:underline underline-offset-2 {klass}"
  >
    <span aria-hidden="true" class="contents">
      {@render tokens()}
    </span>
  </a>
{:else}
  <span class="inline-flex items-center gap-1 min-w-0 {klass}">
    {@render tokens()}
  </span>
{/if}
