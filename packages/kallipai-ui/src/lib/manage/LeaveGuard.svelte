<script lang="ts">
  // Unsaved-changes guard for pending profile edits, shared by every shell.
  // Both hosts mount it next to the shared route view, so it is present on
  // every route and not just the profiles page; that is equivalent because
  // the hook below returns early unless `profilesStore.isDirty`, and that
  // flag turns true only while the profiles page holds unsaved edits, so all
  // other navigations pass through untouched.
  //
  // The decisions live in profiles-view.ts; this component owns only the
  // dialog and the three exits (save and leave, stay, discard and leave).
  //
  // The navigation hook is a port: `$app/navigation` resolves only inside a
  // kit host, so the host passes its `beforeNavigate` in and the component
  // never imports `$app/*`.
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import { untrack } from "svelte";
  import { profilesStore } from "./profiles.svelte.ts";
  import {
    leaveGuardDialogVisible,
    leaveGuardIntercept,
  } from "./profiles-view.ts";
  import { navigate } from "../shell/port.ts";
  import {
    common_cancel,
    manage_profiles_discard,
    manage_profiles_unsaved_body,
    manage_profiles_unsaved_save,
    manage_profiles_unsaved_title,
  } from "../../paraglide/messages.js";
  import type { BeforeNavigateHook } from "../shell/navigate.ts";

  let {
    beforeNavigate,
    fallback,
  }: {
    /** The host's `$app/navigation` beforeNavigate. */
    beforeNavigate: BeforeNavigateHook;
    /** Where to go when the attempt carries no target path: each host passes
     * its own hub. Kit's type allows `to: null`, though its client builds a
     * target for every navigation it intercepts. */
    fallback: () => string;
  } = $props();

  let guard = $state<{ open: boolean; to: string | null }>({
    open: false,
    to: null,
  });

  // The hook is registered once, during initialisation (kit requires it);
  // untrack marks that one-shot read as intentional.
  untrack(() => beforeNavigate)((nav) => {
    if (!leaveGuardIntercept(profilesStore.isDirty, guard.open)) return;
    nav.cancel();
    guard = { open: true, to: nav.to?.url?.pathname ?? fallback() };
  });

  function stay() {
    guard = { open: false, to: null };
  }

  async function leaveTo(target: string | null) {
    guard = { open: false, to: null };
    await navigate(target ?? fallback());
  }

  async function discardAndLeave() {
    if (profilesStore.config) profilesStore.reset();
    await leaveTo(guard.to);
  }

  async function saveAndLeave() {
    await profilesStore.save().catch(() => {});
    // A dangling confirm keeps the dialog open until the user answers it.
    if (profilesStore.pendingDangling !== null) return;
    if (!profilesStore.isDirty) await leaveTo(guard.to);
  }

  function beforeUnloadGuard(e: BeforeUnloadEvent) {
    if (profilesStore.isDirty) e.preventDefault();
  }
</script>

<svelte:window onbeforeunload={beforeUnloadGuard} />

<Dialog
  open={leaveGuardDialogVisible(guard.open, profilesStore.pendingDangling)}
  onOpenChange={(e) => {
    if (!e.open) stay();
  }}
>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-sm p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">
          {manage_profiles_unsaved_title()}
        </Dialog.Title>
        <Dialog.Description class="text-sm opacity-80">
          {manage_profiles_unsaved_body()}
        </Dialog.Description>
        <div class="flex flex-col gap-2">
          <button
            type="button"
            class="btn preset-outlined-primary-500 text-primary-500 transition hover:brightness-110"
            onclick={saveAndLeave}
          >
            {manage_profiles_unsaved_save()}
          </button>
          <button
            type="button"
            class="btn preset-filled-surface-500 text-on-surface-500 transition hover:brightness-110"
            onclick={stay}
          >
            {common_cancel()}
          </button>
          <button
            type="button"
            class="btn text-error-500 hover:preset-filled-error-500 hover:text-on-error-500"
            onclick={discardAndLeave}
          >
            {manage_profiles_discard()}
          </button>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
