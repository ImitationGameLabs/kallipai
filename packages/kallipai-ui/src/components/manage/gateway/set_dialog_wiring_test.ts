import { assert } from "@std/assert";

// Source-read pins for the set dialog's member removal wiring. The card
// must expose a remove action, and the dialog must splice its member
// draft through it. The i18n key itself is verified by the type gate:
// the card imports it from the compiled paraglide modules, and a key
// missing from any locale fails the compile the check task runs on.
const MEMBER_CARD = new URL(
  "../../gateway/GatewayMemberCard.svelte",
  import.meta.url,
);
const SET_DIALOG = new URL("./SetDialog.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the member card carries a remove action wired to an aria label",
  { permissions: { read: [MEMBER_CARD] } },
  () => {
    const src = source(MEMBER_CARD);
    assert(src.includes("onRemove: () => void;"), "the card declares onRemove");
    assert(
      src.includes("aria-label={manage_profiles_set_dialog_remove_member()}"),
      "the remove button's aria label uses the dialog's i18n key",
    );
    assert(
      src.includes("onclick={onRemove}"),
      "the remove button clicks through to onRemove",
    );
  },
);

Deno.test(
  "the dialog splices the member draft on removal and hands it to save",
  { permissions: { read: [SET_DIALOG] } },
  () => {
    const src = source(SET_DIALOG);
    const removeAt = src.indexOf("function removeAt(");
    assert(removeAt !== -1, "the dialog defines removeAt");
    assert(
      src.includes("onRemove={() => removeAt(idx)}"),
      "each member card's onRemove targets its own index",
    );
    // The splice sits inside removeAt, before the draft reassignment.
    assert(
      src.indexOf("next.splice(idx, 1)") > removeAt,
      "removeAt splices the copied list before reassigning the draft",
    );
  },
);

Deno.test(
  "the member card offers the order moves at its edges",
  { permissions: { read: [MEMBER_CARD] } },
  () => {
    const src = source(MEMBER_CARD);
    assert(
      src.includes("aria-label={manage_gateway_set_member_move_up()}"),
      "the up button names its move",
    );
    // The edge refuses the move by disabling the button: a click on the
    // first or last row is not a request that lands nowhere.
    assert(
      src.includes("disabled={isFirst || busy}") &&
        src.includes("onclick={onMoveUp}"),
      "the up button is disabled on the first row and calls onMoveUp",
    );
    assert(
      src.includes("disabled={isLast || busy}") &&
        src.includes("onclick={onMoveDown}"),
      "the down button is disabled on the last row and calls onMoveDown",
    );
    // The card shows the row the set holds, and nothing about where else
    // that row might sit: this dialog edits one set's membership.
    assert(
      src.includes("{member.family}") && src.includes("{member.model}"),
      "the card shows the row's own facts",
    );
  },
);

Deno.test(
  "the dialog explains the order and hands the whole draft back",
  { permissions: { read: [SET_DIALOG] } },
  () => {
    const src = source(SET_DIALOG);
    assert(
      src.includes("manage_gateway_set_members_order_hint()"),
      "the member list says what the order means",
    );
    const submit = src.slice(src.indexOf("function submit()"));
    assert(
      submit.includes("description: descriptionDraft,") &&
        submit.includes("members: memberIds(members),") &&
        submit.includes(
          "...membershipWrites(set.profiles, members, edits, set.name),",
        ),
      "submit hands the description, the order, the membership writes",
    );
    // A re-read of the same set is a partial reseed, not a fresh one:
    // setDraft keeps a landed row and returns an untouched order whole.
    assert(
      src.includes("set === seeded") && src.includes("const next = reseed({"),
      "a re-read of the same set reseeds through setDraft",
    );

    // Either half of that hand-off lost leaves the draft comparing against
    // a seed nobody updates: the call has to learn the seed, and the page
    // has to take back the one the reseed hands it.
    const call = src.slice(src.indexOf("const next = reseed({"));
    const reseedCall = call.slice(0, call.indexOf("});"));
    assert(
      reseedCall.includes("seededIds,"),
      "the reseed learns the seed the draft was built against",
    );
    assert(
      src.includes("seededIds = next.seededIds;"),
      "the dialog takes back the seed the reseed hands it",
    );
  },
);

Deno.test(
  "the dialog wires every card edge and its own edit state",
  { permissions: { read: [SET_DIALOG] } },
  () => {
    const src = source(SET_DIALOG);
    for (const wiring of [
      "isFirst={idx === 0}",
      "isLast={idx === members.length - 1}",
      "onMoveUp={() => step(idx, -1)}",
      "onMoveDown={() => step(idx, 1)}",
      "editDraft={edits[member.profile_id] ?? null}",
    ]) {
      assert(src.includes(wiring), `the card receives ${wiring}`);
    }
  },
);

Deno.test(
  "an impossible field stops the save at its own row",
  { permissions: { read: [MEMBER_CARD, SET_DIALOG] } },
  () => {
    const card = source(MEMBER_CARD);
    assert(
      card.includes("value={editDraft.context_window}") &&
        card.includes("contextProblem(editDraft)") &&
        card.includes("manage_profiles_profile_dialog_invalid_window()"),
      "the field keeps its text and names the problem under itself",
    );
    assert(
      card.includes("modelProblem(editDraft)") &&
        card.includes("manage_profiles_profile_dialog_invalid_model()"),
      "the model field names its problem the same way",
    );
    const dialog = source(SET_DIALOG);
    // A bad draft stops the save by opening the row that holds it: the
    // problem appears under the field, and no draft is dropped.
    assert(
      dialog.includes("firstOffending(members, edits)") &&
        dialog.includes("editingId = offending.profile_id;"),
      "save opens the first offending row instead of dropping it",
    );
    assert(
      card.includes("draggable={!editing && !busy}") &&
        dialog.includes("if (busy) return;"),
      "a save in flight freezes the drag and the step moves",
    );
  },
);
