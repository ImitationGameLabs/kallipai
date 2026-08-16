<script lang="ts" module>
  // The shape of a room member row: the subset of `RoomMemberProfile` this row
  // reads (id/kind/handle/label). Declared structurally so the component does
  // not depend on the lesche-client package's concrete type.
  export type MemberRowMember = {
    readonly id: string;
    readonly kind: "human" | "agent";
    readonly label?: string;
    readonly handle: string;
    /** The agent's tagma_id (absent for humans) -- enables a profile link. */
    readonly tagma_id?: string;
  };
</script>

<script lang="ts">
  // A single room-member row shared by the conversation side panel and the
  // settings roster: an optional online-presence dot, the member's identity
  // (icon + display name + @handle + short id via <SenderIdentity>), and a
  // creator badge. The caller's own row is highlighted with the same filled
  // primary accent as own chat bubbles so it stands out (no "you" text); an
  // offline row recedes via a muted text color (NOT a wrapper `opacity-*`, which
  // would compound with <SenderIdentity>'s own token opacities -- see its
  // contract). Kind is shown by the icon, not a text badge. `online` is `null`
  // on the settings page (no live dot / no dimming).
  import SenderIdentity from "./SenderIdentity.svelte";
  import { profileHref } from "../../lib/room-message.ts";
  import { UserMinus } from "@lucide/svelte";

  let {
    member,
    selfId,
    isCreator = false,
    online = null,
    removable = false,
    onRemove,
  }: {
    member: MemberRowMember;
    selfId: string | null | undefined;
    isCreator?: boolean;
    online?: boolean | null;
    // Whether the viewer may remove this member (creator admin over another
    // member). When true, `onRemove` is invoked by the trailing Remove button.
    // Absent on the conversation side panel, which never offers removal.
    removable?: boolean;
    onRemove?: () => void;
  } = $props();

  const self = $derived(member.id === selfId);
  // Self is highlighted brightest (filled primary, matching own bubbles);
  // otherwise an explicitly-offline row is muted; otherwise the normal tonal
  // surface. Self takes precedence over offline so you can always spot yourself.
  const rowTone = $derived(
    self
      ? "preset-filled-primary-500"
      : online === false
        ? "preset-tonal-surface text-surface-500 dark:text-surface-400"
        : "preset-tonal-surface",
  );
  // Profile link (shared rule): human -> /user/<username>, agent ->
  // /tagma/<tagma_id>; undefined for a degraded handle or an agent without a
  // wire tagma_id.
  const href = $derived(
    profileHref(member.kind, member.handle, member.tagma_id),
  );
</script>

<div
  class="card px-3 py-2 flex items-center justify-between gap-2 text-sm {rowTone}"
>
  <span class="flex items-center gap-2 min-w-0">
    {#if online !== null}
      <!-- The dot carries the accessible name (role="img"); the filled/hollow
        shape distinguishes online/offline beyond color alone (WCAG 1.4.1). -->
      <span
        class="size-2 rounded-full shrink-0 {online
          ? 'bg-success-500'
          : 'border border-surface-400-600 bg-transparent'}"
        role="img"
        aria-label={online ? "online" : "offline"}
      ></span>
    {/if}
    <SenderIdentity
      kind={member.kind}
      handle={member.handle}
      label={member.label}
      {href}
    />
  </span>
  <span class="flex items-center gap-2 shrink-0">
    {#if isCreator}
      <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
        >creator</span
      >
    {/if}
    {#if removable && onRemove}
      <button
        type="button"
        class="size-7 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-error-500 hover:text-on-error-500"
        aria-label="Remove {member.handle}"
        title="Remove {member.handle}"
        onclick={onRemove}
      >
        <UserMinus class="size-4" />
      </button>
    {/if}
  </span>
</div>
