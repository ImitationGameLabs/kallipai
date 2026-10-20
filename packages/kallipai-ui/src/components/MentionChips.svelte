<script lang="ts">
  // The mention chips under a room message body: one capsule per member the
  // sender named, in the sender's order.
  //
  // A presentation leaf. Each capsule re-uses <SenderIdentity>, so the label /
  // `@handle` / short-id vocabulary and the degradation rule (a degraded
  // handle renders verbatim and stays unlinked) keep their ONE home; this
  // component adds only what a chip needs on top of the header: the capsule
  // chrome, and the states the header never draws --
  //   - a mention the roster names nobody for. Two cases, kept apart: the
  //     roster landed and does not hold the id (unknown member + its short id),
  //     versus the roster has not landed yet -- then the chip draws the bare
  //     short id and claims nothing, since the client does not know yet;
  //   - the viewer's own mention (ringed AND carrying the visible word "you").
  // The word is deliberate: color alone must not be the channel that says
  // "this one is you".
  //
  // The container is a list, not a span row: a screen reader then announces how
  // many members were named. `role="list"` keeps that count once the bullet
  // glyphs are styled away.
  import SenderIdentity from "./rooms/SenderIdentity.svelte";
  import { shortIdOf, type MentionChip } from "../lib/room-message.ts";
  import {
    chat_mention_remove_aria,
    chat_mention_self,
    chat_mention_unknown,
    chat_mentions_aria,
  } from "../paraglide/messages.js";

  let {
    chips,
    onRemove,
  }: {
    chips: readonly MentionChip[];
    /** Present, each capsule gains a way out: the composer's picked strip
     *  removes a member here. Absent, the chips only report (a message's
     *  mentions are not editable). */
    onRemove?: (id: string) => void;
  } = $props();
</script>

<ul
  role="list"
  class="mt-1 flex flex-wrap items-center gap-1 list-none p-0"
  aria-label={chat_mentions_aria()}
>
  <!-- Keyed by id: mentionChipsOf folds a repeated id to its first chip, so
  the list is unique without relying on the decoder having de-duplicated. -->
  {#each chips as chip (chip.id)}
    <li
      class="inline-flex items-center gap-1 min-w-0 max-w-full rounded-full border border-black/10 bg-black/5 px-2 py-0.5 text-xs dark:border-white/20 dark:bg-white/10 {chip.self
        ? 'ring-1 ring-primary-500'
        : ''}"
    >
      {#if chip.member}
        <SenderIdentity
          kind={chip.member.kind}
          handle={chip.member.handle}
          label={chip.member.label}
          href={chip.member.href}
        />
      {:else if chip.unnamed === "unknown"}
        <span class="truncate opacity-70"
          >{chat_mention_unknown({ shortId: shortIdOf(chip.id) })}</span
        >
      {:else}
        <!-- The roster has not landed: no words, just the short-id token the
        header uses, so the chip does not call an unknown member unknown. -->
        <span class="font-mono opacity-50 shrink-0">·{shortIdOf(chip.id)}</span>
      {/if}
      {#if chip.self}
        <span class="opacity-60">{chat_mention_self()}</span>
      {/if}

      {#if onRemove}
        <button
          type="button"
          class="shrink-0 rounded p-0.5 opacity-60 hover:bg-surface-200-800 hover:opacity-100 cursor-pointer"
          aria-label={chat_mention_remove_aria({
            name:
              chip.member?.label ?? chip.member?.handle ?? shortIdOf(chip.id),
          })}
          onclick={() => onRemove(chip.id)}
        >
          <span class="text-sm leading-none" aria-hidden="true">×</span>
        </button>
      {/if}
    </li>
  {/each}
</ul>
