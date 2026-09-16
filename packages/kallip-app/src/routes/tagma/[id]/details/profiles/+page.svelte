<script lang="ts">
  // Remote-manage profiles page. The unsaved-changes leave guard lives in
  // this host wrapper (see docs/en/development/frontend.md): the store is the same
  // singleton both shells use, so dirty state and the dangling-confirm
  // handoff behave identically to the direct shell.
  import { beforeNavigate } from "$app/navigation";
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import { OnlineManagePage } from "@kallipai/kallip-ui";
  import { page } from "$app/state";
  import {
    common_cancel,
    leaveGuardDialogVisible,
    leaveGuardIntercept,
    manage_profiles_discard,
    manage_profiles_unsaved_body,
    manage_profiles_unsaved_save,
    manage_profiles_unsaved_title,
    profilesStore,
  } from "@kallipai/kallip-ui";
  import { navigate as shellNavigate } from "@kallipai/kallip-ui";

  let leaveGuard = $state<{ open: boolean; to: string | null }>({
    open: false,
    to: null,
  });

  beforeNavigate((nav) => {
    if (!leaveGuardIntercept(profilesStore.isDirty, leaveGuard.open)) return;
    nav.cancel();
    leaveGuard = {
      open: true,
      to: nav.to?.url?.pathname ?? `/tagma/${page.params.id}/details`,
    };
  });

  function guardStay() {
    leaveGuard = { open: false, to: null };
  }

  async function leaveTo(to: string | null) {
    const target = to ?? `/tagma/${page.params.id}/details`;
    leaveGuard = { open: false, to: null };
    await shellNavigate(target);
  }

  async function guardDiscardAndLeave() {
    if (profilesStore.config) {
      profilesStore.draft = structuredClone(profilesStore.config);
    }
    await leaveTo(leaveGuard.to);
  }

  async function guardSaveAndLeave() {
    await profilesStore.save().catch(() => {});
    if (profilesStore.pendingDangling !== null) return;
    if (!profilesStore.isDirty) await leaveTo(leaveGuard.to);
  }

  function beforeUnloadGuard(e: BeforeUnloadEvent) {
    if (profilesStore.isDirty) e.preventDefault();
  }
</script>

<svelte:window onbeforeunload={beforeUnloadGuard} />

<OnlineManagePage tagmaId={page.params.id ?? ""} page="profiles" />

<Dialog
  open={leaveGuardDialogVisible(leaveGuard.open, profilesStore.pendingDangling)}
  onOpenChange={(e) => {
    if (!e.open) guardStay();
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
            onclick={guardSaveAndLeave}
          >
            {manage_profiles_unsaved_save()}
          </button>
          <button
            type="button"
            class="btn preset-filled-surface-500 text-on-surface-500 transition hover:brightness-110"
            onclick={guardStay}
          >
            {common_cancel()}
          </button>
          <button
            type="button"
            class="btn text-error-500 hover:preset-filled-error-500 hover:text-on-error-500"
            onclick={guardDiscardAndLeave}
          >
            {manage_profiles_discard()}
          </button>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
