<script lang="ts">
  // The unified "add" entry for the Passkeys section, presented as a Skeleton
  // dialog: a low-key trigger opens a modal whose body is a Skeleton Tabs with
  // two panels -- "Another device" (mint a pairing code + QR) and "This device"
  // (enroll another passkey on this browser). The panel area is a fixed-height
  // slot (sized to the tallest state -- the QR view) so switching tabs, or
  // minting a code, does not resize the dialog. Prop-driven and portable; owns
  // only the dialog + the tabs.
  import { Dialog, Portal, Tabs } from "@skeletonlabs/skeleton-svelte";
  import type {
    PasskeyAddHint,
    PairingCodeView,
  } from "../../lib/passkeys.svelte.ts";
  import AddPasskey from "./AddPasskey.svelte";
  import PairAnotherDevice from "./PairAnotherDevice.svelte";
  import {
    settings_add_device_trigger,
    settings_add_device_title,
    settings_add_device_desc,
    settings_another_device,
    settings_this_device,
    common_done,
  } from "../../paraglide/messages.js";

  let {
    adding = false,
    addHint = null,
    onAdd,
    pairingCode = null,
    pairingError = null,
    minting = false,
    onMint,
    onClear,
  }: {
    // Local path (another passkey on this browser). May pass
    // `{ discoverable: true }` to enroll a passwordless credential.
    adding?: boolean;
    addHint?: PasskeyAddHint | null;
    onAdd?: (
      label: string,
      opts?: { discoverable?: boolean },
    ) => Promise<boolean> | boolean | void;
    // Cross-device path (mint a pairing code).
    pairingCode?: PairingCodeView | null;
    pairingError?: string | null;
    minting?: boolean;
    onMint?: () => void | Promise<void>;
    onClear?: () => void;
  } = $props();

  // Controlled open (Skeleton-Svelte Dialog types declare no bindable prop, so
  // we feed `open` back via `onOpenChange`). Escape / backdrop dismiss come for
  // free from the Zag machine defaults.
  let open = $state(false);

  type Mode = "another" | "this";
  // "Another device" is the more common intent; pre-select it.
  let mode = $state<Mode>("another");
</script>

<Dialog {open} onOpenChange={(e) => (open = e.open)}>
  <Dialog.Trigger
    class="text-sm opacity-60 hover:opacity-100 transition-opacity cursor-pointer"
  >
    {settings_add_device_trigger()}
  </Dialog.Trigger>

  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-md p-5 space-y-4"
      >
        <Dialog.Title class="text-lg font-semibold"
          >{settings_add_device_title()}</Dialog.Title
        >
        <Dialog.Description class="sr-only">
          {settings_add_device_desc()}
        </Dialog.Description>

        <Tabs value={mode} onValueChange={(e) => (mode = e.value as Mode)}>
          <Tabs.List class="flex gap-2">
            <Tabs.Trigger
              value="another"
              class="btn btn-sm {mode === 'another'
                ? 'preset-filled-primary-500'
                : 'preset-tonal-surface'}"
            >
              {settings_another_device()}
            </Tabs.Trigger>
            <Tabs.Trigger
              value="this"
              class="btn btn-sm {mode === 'this'
                ? 'preset-filled-primary-500'
                : 'preset-tonal-surface'}"
            >
              {settings_this_device()}
            </Tabs.Trigger>
          </Tabs.List>

          <!-- Fixed-height slot so the dialog never resizes across tabs or when
               a QR appears (sized to the QR view, the tallest state). -->
          <Tabs.Content value="another" class="min-h-[360px] pt-4">
            <PairAnotherDevice
              view={pairingCode}
              error={pairingError}
              {minting}
              {onMint}
              {onClear}
            />
          </Tabs.Content>
          <Tabs.Content value="this" class="min-h-[360px] pt-4">
            <AddPasskey
              busy={adding}
              hint={addHint}
              {onAdd}
              onAdded={() => (open = false)}
            />
          </Tabs.Content>
        </Tabs>

        <div class="flex justify-end">
          <Dialog.CloseTrigger class="btn btn-sm preset-tonal-surface">
            {common_done()}
          </Dialog.CloseTrigger>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
