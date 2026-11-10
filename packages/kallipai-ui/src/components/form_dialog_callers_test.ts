import { assert, assertEquals, assertFalse } from "@std/assert";
import { codeLines } from "../lib/testing/source_pins.ts";

// What each dialog owes the shell. The shell cannot enforce either half from
// inside: a dialog that forgets `busy` still compiles and still renders, and a
// dialog that keeps its own error paragraph renders the message twice. One pin
// over the callers keeps that boundary honest.

const SHELL = new URL("./FormDialog.svelte", import.meta.url);

const DIALOGS = {
  collection: new URL(
    "./manage/gateway/CollectionDialog.svelte",
    import.meta.url,
  ),
  group: new URL("./manage/gateway/GroupDialog.svelte", import.meta.url),
  placement: new URL(
    "./manage/gateway/PlacementDialog.svelte",
    import.meta.url,
  ),
  profile: new URL("./manage/gateway/ProfileDialog.svelte", import.meta.url),
  provider: new URL("./manage/gateway/ProviderDialog.svelte", import.meta.url),
  providerCredential: new URL(
    "./manage/gateway/ProviderCredentialDialog.svelte",
    import.meta.url,
  ),
  setCreate: new URL(
    "./manage/gateway/SetCreateDialog.svelte",
    import.meta.url,
  ),
  set: new URL("./manage/gateway/SetDialog.svelte", import.meta.url),
  createRoom: new URL("./rooms/CreateRoomDialog.svelte", import.meta.url),
  userCredential: new URL(
    "./gateway/UserCredentialDialog.svelte",
    import.meta.url,
  ),
  userGroup: new URL("./gateway/UserGroupDialog.svelte", import.meta.url),
  userProfile: new URL("./gateway/UserProfileDialog.svelte", import.meta.url),
  userProvider: new URL("./gateway/UserProviderDialog.svelte", import.meta.url),
  userPublish: new URL("./gateway/UserPublishDialog.svelte", import.meta.url),
  userSet: new URL("./gateway/UserSetDialog.svelte", import.meta.url),
};

// The dialogs that offer a destructive action beside save. The list is the
// boundary, not a count: any extra action drawn in the shell's body escapes
// the busy state, and a submit key drawn there also submits the same form,
// which the per-file pin below rejects. A destructive action belongs in the
// slot, where the shell owns its place, its error preset and its busy state.
const DESTRUCTIVE = new Set(["providerCredential", "userCredential"]);

for (const [name, url] of Object.entries(DIALOGS)) {
  Deno.test(`the ${name} dialog hands its state to the shell`, () => {
    const src = codeLines(url);
    assertFalse(
      src.includes('role="alert"'),
      "the error line belongs to the shell, so the dialog keeps none of its own",
    );
    const from = src.indexOf("<FormDialog");
    assert(from >= 0, "the dialog renders the shared shell");
    // The opening tag closes on its own line: `\n>` is that line, and a prop
    // expression ending in `>` cannot be mistaken for it.
    const close = src.indexOf("\n>", from);
    assert(close > from, "the opening tag closes");
    const block = src.slice(from, close);
    assert(block.includes("{busy}"), "the shell learns when a save is out");
    assert(
      block.includes("{error}") || /error=\{\w+\}/.test(block),
      "the shell renders the failure",
    );
    assertFalse(
      /error=\{(null|undefined)\}/.test(block),
      "a constant error passes every signature and shows nothing",
    );
    assert(
      block.includes("{canSubmit}") || block.includes("canSubmit={"),
      "the gate reads the caller's verdict rather than a constant",
    );
    assertFalse(
      /canSubmit=\{(true|false)\}/.test(block),
      "a constant handed to the gate passes every signature and guards nothing",
    );
    assert(
      block.includes("destructiveAction=") === DESTRUCTIVE.has(name),
      DESTRUCTIVE.has(name)
        ? "this dialog offers its destructive action through the shell"
        : "a second footer action goes through the shell, not around it",
    );
    // The two set dialogs seed from a state machine that keys on `open` and
    // on the row the page hands back, so a re-read after a failed save keeps
    // the drafts that never landed: an `onOpen` replay would overwrite them.
    const replays = name !== "set" && name !== "userSet";
    assert(
      block.includes("onOpen={reseed}") === replays &&
        !/onOpen=\{(undefined|null)\}/.test(block),
      "every dialog but the set ones replays its drafts on open, and a set" +
        " dialog re-seeds from its own `$effect`",
    );
    if (!replays) {
      // The exception is only an exception because the `$effect` both clears
      // the seed while closed and takes the row again on open; without the
      // clearing, a reopen would keep the last visit's draft.
      assert(
        src.includes("if (!open) {") &&
          src.includes("seeded = null;") &&
          src.includes("seededKey = null;"),
        "a set dialog clears its seed while closed and seeds again on open",
      );
    }
    assertFalse(
      src.includes('type="submit"'),
      "the save key belongs to the shell; a second submit key submits the same form",
    );
    // Blank out attribute expressions first: a `>` inside one would end the
    // tag early and make a typed button look bare.
    const tagSrc = src.replace(/\{[^{}]*\}/g, "{}");
    assertFalse(
      /<button(?![^>]*type=)/.test(tagSrc),
      "a bare <button> submits the form it sits in, so every button carries a type",
    );
  });
}

async function* walk(dir: URL): AsyncGenerator<URL> {
  for await (const entry of Deno.readDir(dir)) {
    const path = new URL(`${entry.name}${entry.isDirectory ? "/" : ""}`, dir);
    if (entry.isDirectory) yield* walk(path);
    else yield path;
  }
}

// Every file under this directory that renders the shell is one of the dialogs
// above: a dialog that arrives without a row would pass every pin by not being
// looked at. The walk starts at `src/components/`, where every caller lives.
Deno.test(
  "every file under src/components that renders the shell is pinned",
  async () => {
    const known = new Set(Object.values(DIALOGS).map((url) => url.href));
    const unlisted: string[] = [];
    for await (const path of walk(new URL(".", import.meta.url))) {
      if (!path.pathname.endsWith(".svelte")) continue;
      if (path.href === SHELL.href) continue;
      if (!codeLines(path).includes("<FormDialog")) continue;
      if (!known.has(path.href)) unlisted.push(path.pathname);
    }
    assert(
      unlisted.length === 0,
      `these render the shell without a row in DIALOGS: ${unlisted.join(", ")}`,
    );
  },
);

Deno.test("the credential dialog sets its hint from typing", () => {
  const src = codeLines(DIALOGS.userCredential);
  assertEquals(
    src.split("oninput={() => (touched = true)}").length - 1,
    2,
    "both fields mark the reveal, since the shell's gate never presses it",
  );
});

Deno.test("the profile dialog's save key carries its number check", () => {
  const src = codeLines(DIALOGS.profile);
  assert(
    src.includes("canSubmit={canSubmit && numbersValid}"),
    "a limit the parser cannot read keeps the save key disabled",
  );
});

Deno.test("the create-room dialog's save key follows the save", () => {
  const src = codeLines(DIALOGS.createRoom);
  assert(
    src.includes(
      "submitLabel={busy ? rooms_creating() : rooms_create_action()}",
    ),
    "a save in flight renames the key, and the shell passes the expression through",
  );
  assert(
    src.includes("rooms_create_failed({ error })"),
    "the dialog builds the failure message, since the shell prints what it is given",
  );
});
