<script lang="ts">
  // The composer's member suggestions: a listbox that floats above the input
  // card. It is a presentation leaf -- the page owns the token, the candidates
  // and the active row (lib/mentions.ts decides them), and this draws the rows
  // plus the cap notice.
  //
  // The field, not this popup, holds the keyboard: the textarea carries
  // `aria-activedescendant` pointing at the active option, so arrow keys never
  // leave the caret. Options are pickable by pointer too, and the mousedown is
  // swallowed so a click does not blur the field the typing continues in.
  import SenderIdentity from "./SenderIdentity.svelte";
  import {
    type MentionTarget,
    MENTIONS_MAX,
    profileHref,
  } from "../../lib/room-message.ts";
  import { MENTION_LIST_ID, mentionOptionId } from "../../lib/mentions.ts";
  import {
    chat_mention_limit,
    chat_mention_picker_aria,
  } from "../../paraglide/messages.js";

  let {
    rows,
    activeIndex,
    limitHit,
    onPick,
  }: {
    /** The candidates on offer, already ranked and capped by the page. */
    rows: readonly MentionTarget[];
    /** Which row the field's `aria-activedescendant` points at. */
    activeIndex: number;
    /** Whether the last pick was refused by the per-message cap. */
    limitHit: boolean;
    onPick: (id: string) => void;
  } = $props();
</script>

<div
  class="absolute inset-x-0 bottom-full z-20 mb-1 overflow-hidden rounded-base border border-surface-200-800 bg-surface-100-900 shadow-lg"
>
  <ul
    id={MENTION_LIST_ID}
    role="listbox"
    aria-label={chat_mention_picker_aria()}
    class="m-0 max-h-72 list-none overflow-auto p-1"
  >
    {#each rows as row, index (row.id)}
      <!-- The keyboard lives on the textarea (aria-activedescendant), so a row
           needs no key handler of its own; the pointer path is here. -->
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <li
        id={mentionOptionId(row.id)}
        role="option"
        aria-selected={index === activeIndex}
        class="flex cursor-pointer items-center gap-1 truncate rounded px-2 py-1 text-sm {index ===
        activeIndex
          ? 'preset-filled-surface-500'
          : 'hover:bg-surface-200-800'}"
        onmousedown={(event) => event.preventDefault()}
        onclick={() => onPick(row.id)}
      >
        <SenderIdentity
          kind={row.kind}
          handle={row.handle}
          label={row.label}
          href={profileHref(row.kind, row.handle, row.tagma_id)}
        />
      </li>
    {/each}
  </ul>
  {#if limitHit}
    <p class="px-2 pb-1 text-xs text-error-500 dark:text-error-400">
      {chat_mention_limit({ max: MENTIONS_MAX })}
    </p>
  {/if}
</div>
