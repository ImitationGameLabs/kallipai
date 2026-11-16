// Thread grouping: what the room transcript draws per discussion, and what it
// does with a reply whose root the window does not hold. The missing-root
// cases are the point: dropping such a row would read as a message that was
// never sent, and saying it started earlier than the window would be a lie
// when the window simply has not walked forward to it yet.

import { assert, assertEquals } from "@std/assert";
import { type RoomLine } from "./room-message.ts";
import {
  declaredThread,
  drawnRootState,
  threadAtEnd,
  threadDiscussion,
  threadEntry,
  threadPreview,
  threadGroups,
  threadMembership,
  threadRows,
  appendThreadPage,
  threadWindowEnd,
  threadPageClaim,
  threadPaging,
  threadRoomBeyondCount,
  roomRowShape,
} from "./threads.ts";

function line(seq: number, thread?: number, failed?: boolean): RoomLine {
  return {
    seq,
    senderId: "u1",
    senderKind: "human",
    senderHandle: "@ada",
    text: `line ${seq}`,
    ...(thread !== undefined ? { thread } : {}),
    ...(failed === true ? { failed: true } : {}),
    createdAt: "2026-10-21T00:00:00Z",
    mine: false,
  };
}

function quoting(seq: number, quote: number): RoomLine {
  return { ...line(seq), quote };
}

Deno.test("a head and its replies form one group", () => {
  const groups = threadGroups([line(1), line(2), line(3, 1), line(4)]);
  assertEquals(groups.length, 3);
  assertEquals(groups[0].root, 1);
  assertEquals(groups[0].head.seq, 1);
  assertEquals(
    groups[0].replies.map((r) => r.seq),
    [3],
  );
  assertEquals(groups[0].rootState, "rooted");
  assertEquals(groups[1].head.seq, 2);
  assertEquals(groups[1].replies.length, 0);
});

Deno.test("a row that only quotes another joins that discussion", () => {
  // A send from the room composer quotes a row without declaring a thread,
  // so the stored row carries the quote alone: the discussion is the one it
  // answers, and the top level keeps the row it quotes.
  const groups = threadGroups([line(1), line(2), quoting(3, 1)]);
  assertEquals(groups.length, 2);
  assertEquals(groups[0].root, 1);
  assertEquals(
    groups[0].replies.map((r) => r.seq),
    [3],
  );
  assertEquals(groups[0].rootState, "rooted");
  assertEquals(groups[1].root, 2);
  assertEquals(groups[1].replies.length, 0);
});

Deno.test(
  "a quote of a reply joins the discussion the reply belongs to",
  () => {
    const groups = threadGroups([line(1), line(2, 1), quoting(3, 2)]);
    assertEquals(groups.length, 1);
    assertEquals(groups[0].root, 1);
    assertEquals(
      groups[0].replies.map((r) => r.seq),
      [2, 3],
    );
  },
);

Deno.test("a quote stored before threads came along walks to the head", () => {
  // The second row quotes the first, so both answer the head the chain ends
  // at rather than the row each names.
  const groups = threadGroups([line(1), quoting(2, 1), quoting(3, 2)]);
  assertEquals(groups.length, 1);
  assertEquals(groups[0].root, 1);
  assertEquals(
    groups[0].replies.map((r) => r.seq),
    [2, 3],
  );
});

Deno.test(
  "a quote naming a row the window does not hold leaves no head",
  () => {
    // Nobody loads the quoted row here, so the discussion's head is out of
    // reach: the grouping still files the quoting row as a reply, and the view
    // keeps the entry rather than drawing that reply as a topic.
    const groups = threadGroups([line(5), quoting(6, 3)]);
    assertEquals(groups.length, 2);
    assertEquals(groups[1].root, 3);
    assertEquals(groups[1].rootState, "older");
    assertEquals(groups[1].head.seq, 6);
    assertEquals(groups[1].replies.length, 0);
  },
);

Deno.test("a cycle of quotes resolves to a name, not a hang", () => {
  const four = quoting(4, 5);
  const five = quoting(5, 4);
  const bySeq = new Map<number, RoomLine>([
    [4, four],
    [5, five],
  ]);
  assertEquals(threadDiscussion(four, bySeq), 5);
  assertEquals(threadDiscussion(five, bySeq), 4);
});

Deno.test("the panel draws a quote-only row the stretch covers", () => {
  const loaded = [line(1), quoting(2, 1)];
  const rows = threadRows(1, loaded, [line(1)], 2);
  assertEquals(
    rows.map((r) => r.seq),
    [1, 2],
  );
});

Deno.test("a root the panel draws is rooted over the window's verdict", () => {
  // The discussion read reaches a root the room's loaded window never had,
  // so the header follows the rows the panel draws, not the grouping.
  assertEquals(drawnRootState(1, [line(1), line(3, 1)], "older"), "rooted");
  assertEquals(drawnRootState(1, [line(3, 1)], "older"), "older");
  assertEquals(drawnRootState(1, [line(3, 1)], "pending"), "pending");
  assertEquals(drawnRootState(1, [line(3, 1)], undefined), "unknown");
});

Deno.test("a group sits where its head sits, and replies stay in order", () => {
  // Out-of-order input: the reply arrives in the array before its root.
  const groups = threadGroups([line(3, 1), line(1), line(4, 1), line(2)]);
  assertEquals(
    groups.map((g) => g.root),
    [1, 2],
  );
  assertEquals(groups[0].head.seq, 1);
  assertEquals(
    groups[0].replies.map((r) => r.seq),
    [3, 4],
  );
  assertEquals(groups[0].rootState, "rooted");
});

Deno.test(
  "a reply whose root is older than the window is drawn, not dropped",
  () => {
    // The window starts at row 5 and row 3's discussion continues into it.
    const groups = threadGroups([line(5), line(6, 3), line(7, 3)]);
    assertEquals(groups.length, 2);
    const orphan = groups[1];
    assertEquals(orphan.root, 3);
    assertEquals(orphan.rootState, "older");
    assertEquals(orphan.head.seq, 6);
    assertEquals(
      orphan.replies.map((r) => r.seq),
      [7],
    );
  },
);

Deno.test(
  "a live reply whose root the walk has not reached is still on its way",
  () => {
    // A live frame carries the declaration only and a negative synthetic seq.
    // Its root is ahead of the window the forward walk has loaded so far, so
    // the start is not older than what is shown -- it simply has not arrived.
    const live = line(-4, 12);
    const groups = threadGroups([line(1), live]);
    assertEquals(groups.length, 2);
    assertEquals(groups[1].root, 12);
    assertEquals(groups[1].rootState, "pending");
    assertEquals(groups[1].head.seq, -4);
  },
);

Deno.test(
  "a discussion joins its loaded root once the window catches up",
  () => {
    // The live reply above, with the walk now holding rows 12 and after.
    const groups = threadGroups([
      line(1),
      line(12),
      line(13, 12),
      line(-4, 12),
    ]);
    assertEquals(groups.length, 2);
    const discussion = groups[1];
    assertEquals(discussion.root, 12);
    assertEquals(discussion.rootState, "rooted");
    assertEquals(discussion.head.seq, 12);
    assertEquals(
      discussion.replies.map((r) => r.seq),
      [13, -4],
    );
  },
);

Deno.test("a live head with no thread heads its own discussion", () => {
  const groups = threadGroups([line(1), line(-7)]);
  assertEquals(groups.length, 2);
  assertEquals(declaredThread(line(-7)), -7);
  assertEquals(groups[1].head.seq, -7);
  assertEquals(groups[1].rootState, "rooted");
});

Deno.test("a thread naming its own seq is a head, not a missing root", () => {
  // Not a shape the relay writes (a head is stored with a NULL thread), but a
  // crafted payload must not turn a row into a reply to itself.
  const groups = threadGroups([line(2, 2)]);
  assertEquals(groups.length, 1);
  assertEquals(groups[0].rootState, "rooted");
  assertEquals(groups[0].replies.length, 0);
});

Deno.test("a root no row can place says nothing at all", () => {
  // The root sits inside the window's bounds but is not held: a dropped frame
  // or a read failure. The view may not call it older or still loading.
  const groups = threadGroups([line(1), line(2), line(3, 1), line(500, 499)]);
  assertEquals(groups[0].repliesExact, true);
  assertEquals(groups[1].repliesExact, true);
  assertEquals(groups[2].rootState, "unknown");
  assertEquals(groups[2].repliesExact, false);
});

Deno.test(
  "a window that starts at row 500 still counts its heads exactly",
  () => {
    // The window reaches the present but not the room's first row: a head that
    // is in it has all of its replies in it, so the count is exact.
    const groups = threadGroups([line(500), line(501, 500), line(502, 500)]);
    assertEquals(groups.length, 1);
    assertEquals(groups[0].replies.length, 2);
    assertEquals(groups[0].repliesExact, true);
  },
);

Deno.test("a failed reply is not counted as a reply the room holds", () => {
  // A failed send stays in the window so the reader can retry or delete it,
  // but the room has no such row: counting it would name a message nobody
  // else ever received.
  const groups = threadGroups([line(1), line(2, 1), line(-5, 1, true)]);
  assertEquals(groups.length, 1);
  assertEquals(groups[0].replies.length, 2);
  assertEquals(groups[0].confirmed.length, 1);
  assertEquals(groups[0].confirmed[0]?.seq, 2);
});

Deno.test("a discussion says why the reader is in it", () => {
  const named = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";
  const root = line(7);
  const reply: RoomLine = { ...line(8, 7), senderId: "u2" };
  const naming: RoomLine = { ...line(9, 7), senderId: "u2", mentions: [named] };
  assertEquals(threadMembership(7, [root, reply], "u1", true), "started");
  assertEquals(threadMembership(7, [root, reply], "u2", true), "replied");
  assertEquals(threadMembership(7, [root, naming], named, true), "mentioned");
  assertEquals(threadMembership(7, [root, reply], "stranger", true), null);
});

Deno.test("a half discussion explains no reason at all", () => {
  // The row that names the reader may sit past the page cap: naming a
  // reason from what did load would explain the wrong one.
  const named = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";
  const naming: RoomLine = { ...line(9, 7), mentions: [named] };
  assertEquals(threadMembership(7, [naming], named, false), null);
  assertEquals(threadMembership(7, [naming], null, true), null);
});

Deno.test("the panel draws a pending reply once, not beside its echo", () => {
  // A send's optimistic line and the row the room numbered are one message:
  // the read that returned the confirmed row replaced the pending one.
  const confirmed = { ...line(9, 7), senderId: "u1", text: "hello" };
  const pending = { ...line(-3, 7), senderId: "u1", text: "hello" };
  assertEquals(threadRows(7, [pending], [confirmed], 9), [confirmed]);
});

Deno.test("the panel keeps the rows a read cannot return", () => {
  const fetched = line(8, 7);
  const pending = line(-3, 7);
  const other = line(-4, 99);
  const head = line(7);
  assertEquals(threadRows(7, [head, pending, other], [fetched], 8), [
    head,
    fetched,
    pending,
  ]);
});

Deno.test(
  "a reply the room placed here is drawn though the read skips it",
  () => {
    // A reply whose declared head did not exist when it was written stores no
    // root, so the read -- filtering on the stored root -- cannot return it,
    // while the room's grouping (by the declared thread) counts it in the
    // discussion's entry. The panel draws the room's row, or the entry counts a
    // reply the panel hides.
    const head = line(2);
    const declared = line(5, 2);
    assertEquals(threadRows(2, [head, declared], [head], 5), [head, declared]);
  },
);

Deno.test("the panel draws only the stretch its pages covered", () => {
  // The pages ran from the root to seq 450, so the room's row below the root
  // and the one past the cursor were never fetched: drawing either would put
  // a row across a stretch nobody read.
  const root = line(300);
  const fetched = line(410, 300);
  const belowRoot = line(120, 300);
  const insideStretch = line(320, 300);
  const pastCursor = line(460, 300);
  assertEquals(
    threadRows(
      300,
      [belowRoot, insideStretch, pastCursor],
      [root, fetched],
      450,
    ),
    [root, insideStretch, fetched],
  );
});

Deno.test("a panel with no page yet draws no numbered row", () => {
  assertEquals(threadRows(300, [line(320, 300)], [], null), []);
});

Deno.test("two numbered rows that read alike are two messages", () => {
  // A seq is unique in its room, so two numbered rows are two messages however
  // alike they read. Only a pending line is folded into the numbered row that
  // echoes it: with the read holding one of them, a content match would drop
  // the other and the panel would draw one row less than the entry counts.
  const echo = { ...line(6, 2), senderId: "u1", text: "again" };
  const later = { ...line(9, 2), senderId: "u1", text: "again" };
  assertEquals(threadRows(2, [later], [echo], 9), [echo, later]);
});

Deno.test(
  "an own reply the room confirmed past the stretch stays drawn",
  () => {
    // The room replaces the pending line with an echo the read has not covered
    // yet: the reader's own message keeps the foot of the window it was sent
    // from, and moves into its place once a page draws it. Somebody else's row
    // out there is still not drawn.
    const mine = { ...line(9, 2), mine: true };
    const theirs = line(10, 2);
    assertEquals(threadRows(2, [line(2), mine, theirs], [line(2)], 4), [
      line(2),
      mine,
    ]);
    assertEquals(threadRows(2, [line(2), mine, theirs], [line(2), mine], 10), [
      line(2),
      mine,
      theirs,
    ]);
  },
);

Deno.test("an older reply with the same words leaves a new send drawn", () => {
  // The fold is identity, not words: while the reader's send is still
  // pending, an older own reply reading the same must not take it off the
  // screen. That page row is one the window already holds, so it is not an
  // echo of the send.
  const older = { ...line(3, 2), mine: true, text: "ok" };
  const pending = { ...line(-1, 2), mine: true, text: "ok" };
  assertEquals(threadRows(2, [line(2), older, pending], [line(2), older], 5), [
    line(2),
    older,
    pending,
  ]);
});
Deno.test(
  "a send folds onto its own echo, not an older one with the words",
  () => {
    // The page brought both the older own reply and the new send's echo. The
    // pending line folds onto the echo, and the two numbered rows keep their
    // own places: a fold that took the older row for the echo would lose one of
    // the three.
    const older = { ...line(3, 2), mine: true, text: "ok" };
    const echo = { ...line(10, 2), mine: true, text: "ok" };
    const pending = { ...line(-1, 2), mine: true, text: "ok" };
    assertEquals(
      threadRows(2, [line(2), older, pending], [line(2), older, echo], 5),
      [line(2), older, echo],
    );
  },
);

Deno.test(
  "an own reply the room confirmed sits above a send still pending",
  () => {
    // Two sends in a row: the room confirmed the first past the stretch the
    // page covered, the second is still pending. The confirmed one is the
    // older message, so it takes the place above, and neither line swallows
    // the other.
    const confirmed = { ...line(10, 2), mine: true, text: "first" };
    const pending = { ...line(-2, 2), mine: true, text: "second" };
    assertEquals(threadRows(2, [line(2), confirmed, pending], [line(2)], 4), [
      line(2),
      confirmed,
      pending,
    ]);
  },
);
Deno.test("a pending line of another discussion takes no claim", () => {
  // The room window carries other discussions' sends too. One of those
  // pending lines must not stand in for the send just made here: the
  // arrival is this discussion's echo, so this pending folds and the other
  // discussion's line stays out of the window.
  const here = { ...line(-1, 2), mine: true, text: "ok" };
  const elsewhere = { ...line(-2, 9), mine: true, text: "ok" };
  const echo = { ...line(11, 2), mine: true, text: "ok" };
  assertEquals(threadRows(2, [line(2), here, elsewhere], [line(2), echo], 5), [
    line(2),
    echo,
  ]);
});

Deno.test(
  "a read that stops on a full page is not the whole discussion",
  () => {
    assertEquals(threadAtEnd(null), false);
    assertEquals(threadAtEnd({ hasMore: true }), false);
    assertEquals(threadAtEnd({ hasMore: false }), true);
  },
);

Deno.test("an entry counts a whole discussion and names its last reply", () => {
  // The failed line is in the window but not in the room, so it is neither
  // counted nor named as the last reply.
  const group = threadGroups([line(1), line(2, 1), line(-9, 1, true)])[0];
  assertEquals(threadEntry(group, false).count, undefined);
  assertEquals(threadEntry(group, false).last, undefined);
  const entry = threadEntry(group, true);
  assertEquals(entry.count, 1);
  assertEquals(entry.last?.seq, 2);
});

Deno.test("an entry with nothing confirmed is the plain invitation", () => {
  // A discussion whose only local row is a failed send has no reply the room
  // holds: a count of zero would advertise a number nobody can see there.
  const group = threadGroups([line(1), line(-9, 1, true)])[0];
  assertEquals(threadEntry(group, true).count, undefined);
  assertEquals(threadEntry(group, true).last, undefined);
});

Deno.test("an entry for a discussion with no loaded root says nothing", () => {
  const group = threadGroups([line(5), line(6, 3)])[1];
  assertEquals(threadEntry(group, true).count, undefined);
  assertEquals(threadEntry(group, true).last, undefined);
});

Deno.test(
  "a window carries past its last page over rows the room holds",
  () => {
    // A reply lands after the panel read to the end of the discussion: the
    // reader's own send, or somebody else's. Every seq between is in the room
    // window, so no stretch was skipped and the row belongs in the panel.
    const loaded = [line(1), line(2, 1), line(3, 1), line(4, 1)];
    assertEquals(threadWindowEnd(2, loaded), 4);
    const drawn = threadRows(1, loaded, [line(1), line(2, 1)], 4);
    assertEquals(
      drawn.map((row) => row.seq),
      [1, 2, 3, 4],
    );
  },
);

Deno.test("a window stops at the first seq the room does not hold", () => {
  // A room window that has not caught up cannot vouch for the rows past its
  // own gap: carrying the stretch over it would draw a jump the reader never
  // made, so the row waits until the missing ones arrive.
  const loaded = [line(1), line(2, 1), line(5, 1)];
  assertEquals(threadWindowEnd(2, loaded), 2);
  // The row past the gap waits for the pages it declares: one for this
  // discussion returns it, and drawing it here would jump the stretch.
  assertEquals(threadRows(1, loaded, [line(1), line(2, 1)], 2).length, 2);
});

Deno.test(
  "a page appended keeps its order and drops what the window holds",
  () => {
    const first = { lines: [line(1), line(2, 1)], lastSeq: 2, hasMore: true };
    const next = {
      lines: [line(2, 1), line(3, 1)],
      lastSeq: 3,
      hasMore: false,
    };
    const read = appendThreadPage(first, next);
    assertEquals(
      read.lines.map((row) => row.seq),
      [1, 2, 3],
    );
    assertEquals(read.lastSeq, 3);
    assertEquals(read.hasMore, false);
  },
);

Deno.test("a page that names no cursor leaves the one the window had", () => {
  const first = { lines: [line(1)], lastSeq: 1, hasMore: true };
  const empty = { lines: [], lastSeq: null, hasMore: false };
  const read = appendThreadPage(first, empty);
  assertEquals(read.lastSeq, 1);
  assertEquals(
    read.lines.map((row) => row.seq),
    [1],
  );
});

Deno.test(
  "a reply the room took after the read is a page worth asking for",
  () => {
    // The discussion was read to its end (its last page was short), then the
    // room moved on and somebody replied to it: the panel draws the stretch
    // its pages covered, so that row sits outside it and only a page brings it in.
    const loaded = [line(1), line(2, 1), line(3, 1), line(4690), line(4691, 1)];
    assertEquals(threadRoomBeyondCount(1, 3, loaded), 1);
  },
);

Deno.test("a room row of another discussion past the stretch is not", () => {
  const loaded = [line(1), line(2, 1), line(4690), line(4691, 7)];
  assertEquals(threadRoomBeyondCount(1, 2, loaded), 0);
});

Deno.test("a stretch the room carries with no gap asks for no page", () => {
  // Every seq between the cursor and the row is held, so threadWindowEnd
  // carries the panel to it: the row is drawn and nothing is left to ask for.
  const loaded = [line(1), line(2, 1), line(3, 1)];
  assertEquals(threadWindowEnd(2, loaded), 3);
  assertEquals(threadRoomBeyondCount(1, threadWindowEnd(2, loaded), loaded), 0);
});

Deno.test("a panel with no read has nothing to ask past", () => {
  const loaded = [line(1), line(2, 1)];
  assertEquals(threadRoomBeyondCount(1, null, loaded), 0);
});

Deno.test(
  "a short page under a row the room still holds is its last word",
  () => {
    // The discussion was read to its end and the room still shows a reply past
    // the stretch those pages covered: no page starts below that reply, so a
    // second ask at the same cursor answers with the same short page.
    const loaded = [line(1), line(2, 1), line(4690), line(4691, 1)];
    assertEquals(threadRoomBeyondCount(1, 2, loaded), 1);
    assertEquals(threadPageClaim({ hasMore: false }, 1), 1);
  },
);

Deno.test("a short page over another discussion's rows is not spent", () => {
  const loaded = [line(1), line(2, 1), line(4690), line(4691, 7)];
  assertEquals(threadRoomBeyondCount(1, 2, loaded), 0);
  assertEquals(threadRoomBeyondCount(7, 2, loaded), 1);
});

Deno.test("a row only a quote places is drawn, not asked for", () => {
  // A hole this row falls through: it answers row 1 but declares no root, so
  // no page for discussion 1 can bring it back, and counting it would ask for
  // a page that answers with what the reader already has. Leaving it out of
  // the panel too would draw it nowhere at all, whoever sent it.
  const loaded = [line(1), line(2, 1), quoting(4691, 1)];
  assertEquals(threadRoomBeyondCount(1, 2, loaded), 0);
  assertEquals(
    threadRows(1, loaded, [], 2).map((row) => row.seq),
    [1, 2, 4691],
  );
});

Deno.test(
  "a short page with nothing of this discussion past it is its end",
  () => {
    // Nothing of this discussion is left over the stretch, so there is nothing
    // to explain and nothing to stop asking for: it ended where the page did.
    const loaded = [line(1), line(2, 1), line(3, 1)];
    assertEquals(threadRoomBeyondCount(1, 3, loaded), 0);
  },
);

Deno.test("a full page is never the last word for its cursor", () => {
  const loaded = [line(1), line(2, 1), line(4691, 1)];
  assertEquals(threadRoomBeyondCount(1, 2, loaded), 1);
  assertEquals(threadPageClaim({ hasMore: true }, 1), null);
});

Deno.test("a read that named no cursor has no stretch to be past", () => {
  const loaded = [line(1), line(2, 1)];
  assertEquals(threadRoomBeyondCount(1, null, loaded), 0);
});

Deno.test(
  "a spent page shuts the gate until the room takes another row",
  () => {
    // The verdict is taken back, not latched: the gate stays shut while the room
    // shows the rows the short page already answered for, opens for one more page
    // when the room takes one of this discussion, and shuts again on the short
    // page that answers for the new count. A latch would keep that reply out of
    // the panel until the discussion was opened again.
    const read = { hasMore: false };
    const rows = [
      line(1),
      line(2, 1),
      line(4690),
      line(4691, 1),
      line(4692, 1),
    ];
    const claim = threadPageClaim(read, threadRoomBeyondCount(1, 2, rows));
    assertEquals(claim, 2);
    const shut = threadPaging(read, threadRoomBeyondCount(1, 2, rows), claim);
    assertEquals(shut.spent, true);
    assertEquals(shut.more, false);
    const grown = [...rows, line(4700, 1)];
    const open = threadPaging(read, threadRoomBeyondCount(1, 2, grown), claim);
    assertEquals(open.spent, false);
    assertEquals(open.more, true);
    const shutAgain = threadPaging(
      read,
      threadRoomBeyondCount(1, 2, grown),
      threadPageClaim(read, threadRoomBeyondCount(1, 2, grown)),
    );
    assertEquals(shutAgain.spent, true);
    assertEquals(shutAgain.more, false);
    assertEquals(threadPaging(null, 0, 3).more, false);
  },
);

Deno.test("a page asked for before the room took a row earns one more", () => {
  // The count that keys the verdict is taken when the page is asked for. A
  // reply the room takes while the page is in flight is not one the page
  // answered for, so the claim stands one short of the room and the gate opens
  // for the page that brings that reply in.
  const read = { hasMore: false };
  const askedFor = [line(1), line(2, 1), line(4691, 1)];
  const landed = [...askedFor, line(4692, 1)];
  const claim = threadPageClaim(read, threadRoomBeyondCount(1, 2, askedFor));
  assertEquals(claim, 1);
  const gate = threadPaging(read, threadRoomBeyondCount(1, 2, landed), claim);
  assertEquals(gate.spent, false);
  assertEquals(gate.more, true);
});

Deno.test("a discussion with replies is drawn as its own entry", () => {
  const [group] = threadGroups([line(1), line(2, 1)]);
  assertEquals(roomRowShape(group), "thread");
});

Deno.test("a message nobody answered is drawn plain", () => {
  const [group] = threadGroups([line(1)]);
  assertEquals(roomRowShape(group), "plain");
});

Deno.test(
  "a discussion whose head is out of the window keeps the button",
  () => {
    // No body is drawn for this group, so there is nothing to make clickable:
    // the entry button and its note stay the only way in.
    const [group] = threadGroups([line(5, 99)]);
    assert(group.rootState !== "rooted", "the head is not in the window");
    assertEquals(roomRowShape(group), "entry");
  },
);

Deno.test(
  "a discussion that fell off the top of the window keeps the button",
  () => {
    // The head is below every row the window holds, so it is older than the
    // window, not still ahead of it: the reader can still open it.
    const [group] = threadGroups([line(10, 3), line(11)]);
    assertEquals(group.rootState, "older");
    assertEquals(roomRowShape(group), "entry");
  },
);

Deno.test("a discussion whose only reply has failed is still a bubble", () => {
  // The reply is drawn even though it never landed: the reader keeps its
  // retry, and the summary still opens the discussion.
  const [group] = threadGroups([line(1), line(-1, 1, true)]);
  assertEquals(group.rootState, "rooted");
  assertEquals(group.replies.length, 1);
  assertEquals(roomRowShape(group), "thread");
});

Deno.test("an entry previews the start of its discussion", () => {
  // The preview answers what the discussion is about, so it takes the first
  // replies in transcript order; opening the page carries on from there.
  const [group] = threadGroups([
    line(1),
    line(2, 1),
    line(3, 1),
    line(4, 1),
    line(5, 1),
  ]);
  const preview = threadPreview(group);
  assertEquals(
    preview.rows.map((row) => row.seq),
    [2, 3, 4],
  );
});

Deno.test("a preview shows only replies the room holds", () => {
  // A pending line of the reader's own and a failed one sit in the window but
  // not in the room: a preview is exactly where one of those could pass for a
  // message somebody else received.
  const [group] = threadGroups([
    line(1),
    line(-2, 1),
    line(-3, 1, true),
    line(4, 1),
  ]);
  const preview = threadPreview(group);
  assertEquals(
    preview.rows.map((row) => row.seq),
    [4],
  );
});

Deno.test("a preview skips a reply with nothing to say", () => {
  // An empty body is a gap in a list of lines, not a message; skipping it also
  // keeps it from taking one of the places the preview has.
  const [group] = threadGroups([
    line(1),
    { ...line(2, 1), text: "   " },
    line(3, 1),
  ]);
  const preview = threadPreview(group);
  assertEquals(
    preview.rows.map((row) => row.seq),
    [3],
  );
});

Deno.test("a preview does not wait for the window to catch up", () => {
  // The window is a prefix, so the replies it holds of a discussion are that
  // discussion's first ones; a count waits for the window to reach the present,
  // a preview does not.
  const [group] = threadGroups([line(1), line(2, 1), line(3, 1)]);
  assertEquals(
    threadPreview(group).rows.map((row) => row.seq),
    [2, 3],
  );
});

Deno.test(
  "a preview draws nothing when the head is not among the lines held",
  () => {
    // Two ways the head can be missing while one of its replies is in the
    // window, so a row is there to draw and the gate is the only reason not
    // to: calling line 7 the start would make a stretch of the middle look
    // like one. Below the window, then above it.
    const group = threadGroups([line(6, 3), line(7, 3)])[0];
    assertEquals(group.repliesExact, false);
    assertEquals(
      group.confirmed.map((row) => row.seq),
      [7],
    );
    assertEquals(threadPreview(group).rows.length, 0);
    const ahead = threadGroups([line(6, 9), line(7, 9)])[0];
    assertEquals(ahead.repliesExact, false);
    assertEquals(
      ahead.confirmed.map((row) => row.seq),
      [7],
    );
    assertEquals(threadPreview(ahead).rows.length, 0);
  },
);
