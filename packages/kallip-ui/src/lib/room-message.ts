// The room-message plaintext codec. A room's message payload is a
// JSON-serialized `RoomMessage { text }` (rooms and the bilateral 1:1 path are
// disjoint address spaces; a room message is just text -- no `req_id`/ack), per
// crates/platform/kallip-lesche-common/src/message.rs:
//   pub struct RoomMessage { pub text: String }
// The tagma's room inbound (`relay/mod.rs::handle_room_message`) does
// `serde_json::from_slice::<RoomMessage>`. So a browser sending into a room
// serializes `{ text }`, and a browser rendering an inbound room frame decodes
// the same JSON to read the text.
//
// Rooms are plaintext server-readable: the browser base64s this JSON and posts
// it as the envelope payload; the lesche stores + relays it opaquely and
// enforces member access. `RoomMessage` is stated only here on the TS side. The
// wire shape is a coordinated Rust+TS contract: a Rust change is a same-commit
// change on both sides.

/** A decoded inbound room message. A payload that is not a `{ text }` object
 * (malformed JSON, or a future fielded shape) is surfaced as `unknown` so the
 * transcript can warn-drop it rather than mis-render. */
export type RoomMessage =
  | { readonly op: "message"; readonly text: string }
  | { readonly op: "unknown"; readonly raw: string };

/** Serialize a chat line for the room wire. The result is the plaintext payload
 * the sender base64s into the envelope. */
export function encodeRoomSendMessage(text: string): string {
  return JSON.stringify({ text });
}

/** Decode an inbound room-message plaintext. Tolerant: a payload without a
 * string `text` (malformed JSON, or a future shape) returns
 * `{ op: "unknown", raw }` so the caller can warn-drop it instead of throwing
 * on a single bad frame. */
export function decodeRoomMessage(plaintext: Uint8Array): RoomMessage {
  const raw = new TextDecoder().decode(plaintext);
  let value: { text?: unknown };
  try {
    value = JSON.parse(raw);
  } catch {
    return { op: "unknown", raw };
  }
  if (typeof value.text === "string") {
    return { op: "message", text: value.text };
  }
  return { op: "unknown", raw };
}

/** One rendered room line. `seq` is the lesche room-message sequence (stable,
 * monotonic) for confirmed lines, or a synthetic NEGATIVE value for an
 * optimistic send / a live frame not yet reconciled with history. `failed` is
 * set on a MINE optimistic line whose POST failed -- the line stays (the user's
 * input is preserved) and the view offers a retry. Pure data -- declared here
 * (not in the reactive store) so the dedup logic below is unit-testable without
 * a Svelte runtime. */
export interface RoomLine {
  readonly seq: number;
  readonly senderId: string;
  /** Sender kind (human vs agent); selects the Cpu/User icon rendered by
   * <SenderIdentity>. */
  readonly senderKind: "human" | "agent";
  /** The relay-stamped authoritative STABLE handle (NOT a display name):
   * `@<username>` for a human, `<id-prefix>@<owner-username>` for an agent.
   * The relay is the sole source of room identity, so this is rendered verbatim
   * (never the client-supplied handle); the mutable display name is resolved
   * separately (roster). `parseParticipantHandle` splits this into the
   * `@handle` + short-id tokens the view renders. */
  readonly senderHandle: string;
  /** For an agent sender, its `tagma_id` (relay-stamped on the wire) so a
   * message header can deep-link to that tagma's profile without reversing the
   * one-way participant id. Undefined for humans and for frames that did not
   * carry it. */
  readonly senderTagmaId?: string;
  readonly text: string;
  readonly createdAt: string;
  readonly mine: boolean;
  readonly failed?: boolean;
}

/** The decomposed stable handle: the `@`-prefixed owner/user handle to render,
 * plus -- for agents only -- the unforgeable short participant-id prefix shown
 * as a separate token. */
export interface ParsedHandle {
  /** The owner/user handle, `@`-prefixed on the well-formed path. Malformed
   * agent input (no `@`) degrades to the raw string unchanged (no fabricated
   * `@`). */
  readonly handle: string;
  /** The agent's short participant-id prefix (agents only). */
  readonly shortId?: string;
}

/** Profile deep-link for a sender. Human -> `/user/<username>` ONLY when the
 * relay resolved a real `@username` (a degraded `user <prefix>` handle has no
 * `@`, so no link). Agent -> `/tagma/<tagma_id>` only when the wire carried
 * the tagma_id. `undefined` otherwise (no broken link). Shared by the room
 * message header and the roster row so the degradation rule has one home.
 *
 * Frontend profile routes are deliberately SINGULAR (`/user/`, `/tagma/`) — the
 * user-facing-URL convention — while the backing API stays RESTful plural
 * (`/v1/users`, `/v1/tagmata`); the two namespaces are independent. */
export function profileHref(
  kind: "human" | "agent",
  handle: string,
  tagmaId?: string,
): string | undefined {
  if (kind === "human") {
    return handle.startsWith("@")
      ? `/user/${encodeURIComponent(handle.slice(1))}`
      : undefined;
  }
  return tagmaId ? `/tagma/${encodeURIComponent(tagmaId)}` : undefined;
}

/** Decompose a relay-stamped stable `handle` into its display tokens.
 *
 * The handle is `<id-prefix>@<owner-username>` for an agent, `@<username>` for
 * a resolved human, or the degraded `"user <id-prefix>"` form for a human the
 * registry did not resolve (built in
 * `crates/platform/kallip-lesche/src/identity.rs`; the optimistic local line
 * emits the same degraded form). Splitting on the FIRST `@` is safe because of
 * two server-side invariants:
 *   - `short_prefix` is `chars().take(6)` of a server-derived participant id --
 *     no `@` (identity.rs).
 *   - an agora username is `[a-z0-9-]` only (single interior hyphens), never
 *     `@` (`crates/platform/kallip-agora/src/username.rs`).
 * A change to either is a flagged contract break, not a silent mis-parse here.
 * Malformed input (no `@`, or an agent handle without one) degrades to
 * `{ handle: raw }` so a bad frame never crashes render. Pure so it is unit-
 * testable without a Svelte runtime; the structured component renders these
 * tokens separately (icon + label + `@handle` + short-id). */
export function parseParticipantHandle(
  raw: string,
  kind: "human" | "agent",
): ParsedHandle {
  if (kind === "agent") {
    const at = raw.indexOf("@");
    if (at > 0) {
      return { handle: "@" + raw.slice(at + 1), shortId: raw.slice(0, at) };
    }
    return { handle: raw };
  }
  // human: a registry-resolved handle is already `@<username>` and passes
  // through. The degraded `"user <id-prefix>"` form (a registry miss at the
  // relay, or the optimistic local line) is passed through VERBATIM -- never
  // fabricated into an `@`, which would forge a username that cannot exist
  // (the agora charset is `[a-z0-9-]`, no spaces).
  return { handle: raw };
}

/** Append a room line to a transcript with pending-line dedup. When a CONFIRMED
 * line (positive seq) arrives whose `(senderId, text)` matches a prior pending
 * line (negative seq), the pending line is REPLACED -- the live frame rendered
 * ahead of its history echo (an optimistic send OR a received live frame), and
 * the echo lands at its real seq. The sender disambiguates: my echo collapses
 * only my pending, another member's echo collapses only theirs. This is
 * `mine`-agnostic on purpose: a received live frame is appended with a
 * synthetic seq then re-appears from history, and would otherwise double. A
 * pending line never collapses another pending line (two same-text sends both
 * show until their real seqs land).
 *
 * Symmetric guard: a pending line (negative seq) is DROPPED when a CONFIRMED
 * line with the same `(senderId, text)` is already rendered. This closes the
 * reverse race where a history fetch wins over SSE delivery and the late live
 * frame would otherwise duplicate an already-shown row (the live frame bypasses
 * the seq guard). A genuinely new same-text message is not lost: its own
 * positive-seq echo appends normally (no pending twin to collapse against).
 * Keys on `senderId` so a different member's same-text row is safe. Returns a
 * fresh array (the store reassigns its reactive `lines`). Pure: unit-tested in
 * room-message_test.ts. */
export function appendRoomLine(
  lines: readonly RoomLine[],
  line: RoomLine,
): RoomLine[] {
  if (line.seq >= 0) {
    const pendingIdx = lines.findIndex(
      (l) => l.seq < 0 && l.senderId === line.senderId && l.text === line.text,
    );
    if (pendingIdx >= 0) {
      const next = lines.slice();
      next[pendingIdx] = line;
      return next;
    }
  } else {
    // A live/synthetic frame: drop it if its confirmed echo is already shown
    // (history fetch won the race over SSE delivery).
    const alreadyConfirmed = lines.some(
      (l) => l.seq >= 0 && l.senderId === line.senderId && l.text === line.text,
    );
    if (alreadyConfirmed) {
      return lines.slice();
    }
  }
  return [...lines, line];
}
