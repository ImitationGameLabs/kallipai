import { assert } from "@std/assert";

// Source pins for the user face's set dialog: the member pool comes from
// the collection, the picker names the set an add would take a row out of,
// and the create form asks for a name an edit already has. The save sequence itself is
// tested in lib/manage/gateway/saveSet_test.ts; what it hands over is
// pinned here and in lib/gateway/userSetDraft_test.ts.
const DIALOG = new URL("./UserSetDialog.svelte", import.meta.url);
const CARD = new URL("./GatewayMemberCard.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the member picker reads its pool from the lib and says when it is empty",
  { permissions: { read: [DIALOG] } },
  () => {
    const src = source(DIALOG);
    // The pool rule itself is tested in lib/gateway/userSetDraft_test.ts;
    // this pins the wiring, so the component cannot grow its own filter.
    assert(
      src.includes("profiles === null ? [] : poolOf(profiles, members)"),
      "the pool is the lib's call, not a filter in the component",
    );
    // An empty pool is a state the user can read, but only once the rows
    // are read: an unread row set must not claim the pool is empty.
    assert(
      src.includes("{#if profiles !== null && pool.length === 0}") &&
        src.includes("{user_gateway_set_add_member_empty()}"),
      "an empty pool says why there is nothing to add, once the rows are read",
    );
    assert(
      src.includes("{#each pool as row (row.profile_id)}"),
      "the picker lists the pool",
    );
  },
);

Deno.test(
  "a create needs a name and an edit keeps the set's own",
  { permissions: { read: [DIALOG] } },
  () => {
    const src = source(DIALOG);
    assert(
      src.includes('!busy && (set !== null || nameDraft.trim() !== "")'),
      "the button greys out until a create has a name",
    );
    assert(
      src.includes("name: set ? set.name : nameDraft.trim(),"),
      "an edit sends the set's name, a create the typed one",
    );
    // A create dialog that reopens after a refused save keeps what was
    // typed: the seed runs once per open, not on every parent render.
    assert(
      src.includes('const CREATE_KEY = "new set";') &&
        src.includes("if (seededKey === CREATE_KEY) return;"),
      "the create seed speaks once per open",
    );
  },
);

Deno.test(
  "the dialog reseeds through setDraft and hands the changed rows",
  { permissions: { read: [DIALOG] } },
  () => {
    const src = source(DIALOG);
    assert(
      src.includes("set === seeded") && src.includes("const next = reseed({"),
      "a re-read of the same set reseeds through setDraft",
    );
    // Read the reseed call itself: it has to learn which rows landed and
    // which ids the draft was seeded with (a member picked after the seed
    // is not in the registry yet, so reseed cannot take it back out).
    const call = src.slice(src.indexOf("const next = reseed({"));
    const reseedCall = call.slice(0, call.indexOf("});"));
    assert(
      reseedCall.includes("landed,") && reseedCall.includes("seededIds,"),
      "the reseed learns which rows landed and which the seed carried",
    );

    // The page takes that seed back: without this line the dialog keeps
    // comparing an order against a seed the registry has moved past.
    assert(
      src.includes("seededIds = next.seededIds;"),
      "the dialog takes back the seed the reseed hands it",
    );
    // A create hands no write at all: the page creates the set first and
    // then walks the same sequence from an empty original, so the create
    // request carries the membership and nothing else.
    const submit = src.slice(src.indexOf("function submit()"));
    assert(
      submit.includes("{ updates: [], joins: [], leaves: [] }") &&
        submit.includes(
          "membershipWrites(set.profiles, members, edits, set.name)",
        ),
      "a create hands no writes, an edit hands the shared three lists",
    );
    // The picker names the set an add would move the row out of, using the
    // row's own field: no second reading of the registry.
    assert(
      src.includes(
        "user_gateway_set_add_member_moves({ set: pickedRow.set })",
      ) && src.includes("pickedRow !== null && pickedRow.set !== null"),
      "the note appears only for a pick that belongs to a set",
    );
    assert(
      src.includes("editable={set !== null}") &&
        src.includes("if (busy || set === null) return;"),
      "a create offers no inline form: nothing exists to PUT yet",
    );
  },
);

Deno.test(
  "the card carries no membership badge and a pick says where it comes from",
  { permissions: { read: [DIALOG, CARD] } },
  () => {
    const dialog = source(DIALOG);
    // A row's set is the registry's fact, not a card badge: the member list
    // only ever shows rows the set holds.
    assert(
      !source(CARD).includes("badge"),
      "the member card shows no membership badge",
    );
    assert(
      dialog.includes(
        "row.set === null ? row.profile_id : `${row.profile_id} (${row.set})`",
      ),
      "a picker row that belongs to a set names it",
    );
  },
);

Deno.test(
  "the dialog closes on a drop the user did not confirm",
  { permissions: { read: [DIALOG] } },
  () => {
    const src = source(DIALOG);
    assert(
      src.includes("if (!e.open && !busy) onCancel();"),
      "a backdrop click while a save runs is ignored",
    );
  },
);
