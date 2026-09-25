<script lang="ts">
  import { settings_dark_mode } from "../paraglide/messages.js";
  import { Switch } from "@skeletonlabs/skeleton-svelte";

  // Mirrors the boot script in each app.html: manual choice (localStorage)
  // wins, otherwise follow the OS preference. The app.html script has already
  // set data-mode before hydration, so this only syncs the control's state.
  let checked = $state(false);

  $effect(() => {
    let stored: string | null = null;
    try {
      stored = localStorage.getItem("mode");
    } catch {
      // Storage blocked (private mode); follow the OS preference.
    }
    checked =
      stored === "dark" ||
      (stored === null && matchMedia("(prefers-color-scheme: dark)").matches);
  });

  function onCheckedChange(event: { checked: boolean }) {
    const mode = event.checked ? "dark" : "light";
    document.documentElement.dataset.mode = mode;
    localStorage.setItem("mode", mode);
    checked = event.checked;
  }
</script>

<Switch {checked} {onCheckedChange}>
  <Switch.Control>
    <Switch.Thumb />
  </Switch.Control>
  <Switch.HiddenInput aria-label={settings_dark_mode()} />
</Switch>
