// The room-message plaintext codec. A room's message payload is a
// JSON-serialized `RoomMessage` (rooms and the bilateral 1:1 path are
// disjoint address spaces; text, an optional attachment, an optional quoted
// seq, and the participant ids it mentions -- no `req_id`/ack), per
// crates/platform/kallipai-lesche-common/src/message.rs:
//   pub struct RoomMessage {
//     pub text: String,
//     pub attachment: Option<FileAttachment>,
//     pub quote: Option<i64>,
//     pub mentions: Vec<MemberId>,  // empty unless the sender named members
//   }
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

import type { FileAttachment } from "@kallipai/kallipai-common";
import {
  chat_message_label_quoting_timed,
  chat_message_label_timed,
  sender_agent_aria,
  sender_user_aria,
} from "../paraglide/messages.js";
import { authorName, jumpAnnouncement } from "./rowSpeech.ts";

/** A decoded inbound room message. A payload that is not a `{ text }` object
 * (malformed JSON, or a future fielded shape) is surfaced as `unknown` so the
 * transcript can warn-drop it rather than mis-render. */
export type RoomMessage =
  | {
      readonly op: "message";
      readonly text: string;
      readonly attachment?: FileAttachment;
      /** The room seq this message quotes, when the sender quoted one. */
      readonly quote?: number;
      /** The participant ids this message names, when the sender named any.
       *  Ids only: the handle is resolved against the roster at render time, so
       *  a rename or a departed member still reads as the same participant. */
      readonly mentions?: readonly string[];
    }
  | { readonly op: "unknown"; readonly raw: string };

/** The optional parts of an outbound room line, as one object so a caller that
 *  names members or quotes a row but sends no file does not spell an empty
 *  attachment. */
export type RoomSendOptions = {
  readonly attachment?: FileAttachment;
  /** The room seq this line answers. Only the position crosses the wire, so a
   *  quoted row that leaves the window still shows as a quote rather than a
   *  copy of text someone else wrote. */
  readonly quote?: number;
  readonly mentions?: readonly string[];
};

/** The quote a send carries: undefined when it quotes nothing, the seq when it
 *  does, and a throw for anything that cannot name a row. Strict for the same
 *  reason `checkedMentions` is: a quote that silently ships as no quote is a
 *  message quietly claiming to answer nothing. */
export function checkedQuote(seq: number | undefined): number | undefined {
  if (seq === undefined) return undefined;
  if (!Number.isInteger(seq) || seq <= 0) {
    throw new Error(`not a quoted room seq: ${seq}`);
  }
  return seq;
}

/** The mention ids a send carries: every id in the accepted shape, each settled
 *  on its canonical lowercase spelling, repeats folded into the first place, so
 *  the cap counts members rather than entries. It throws when it cannot place a
 *  member the caller named -- a bad shape, or more members than `MENTIONS_MAX`
 *  allows -- because a send that quietly drops a picked member looks like a
 *  mention that names nobody. A repeat names a member already placed, so it
 *  folds rather than failing the send. Exported so a caller can settle a list
 *  once: the room store does, before it draws the optimistic line, so the
 *  bubble and the wire carry the same names. */
export function checkedMentions(ids: readonly string[] | undefined): string[] {
  if (ids === undefined) return [];
  const out: string[] = [];
  for (const raw of ids) {
    const id = raw.toLowerCase();
    if (!PARTICIPANT_ID.test(id)) {
      throw new Error(`not a participant id: ${raw}`);
    }
    if (out.includes(id)) continue;
    out.push(id);
    // The cap is judged inside the fold, the way the Rust side does it: a
    // list past it stops here instead of folding every entry first.
    if (out.length > MENTIONS_MAX) {
      throw new Error(`a send names more than ${MENTIONS_MAX} members`);
    }
  }
  return out;
}

/** The `mentions` field a payload or a line carries: present only when the
 * sender named someone, absent otherwise. Every site that writes the field
 * goes through this one shape, so "an unnamed send carries no key" is one
 * decision instead of four spreads that can drift apart. */
export function mentionsField(ids: readonly string[] | undefined): {
  mentions?: readonly string[];
} {
  return ids !== undefined && ids.length > 0 ? { mentions: ids } : {};
}

/** The `quote` field a payload or a line carries: present only when the line
 *  answers another one. The twin of `mentionsField`, and for the same reason --
 *  every site that writes the field goes through one shape, so an unquoted
 *  send cannot grow a `quote: undefined` key on some path and not another. */
export function quoteField(seq: number | undefined): { quote?: number } {
  return seq !== undefined ? { quote: seq } : {};
}

/** Serialize a chat line for the room wire. The result is the plaintext payload
 * the sender base64s into the envelope. The mention list and the quoted seq are
 * taken strictly (see `checkedMentions` / `checkedQuote`): a send names the
 * members it picked, and quotes the row it answers, or it throws. */
export function encodeRoomSendMessage(
  text: string,
  options?: RoomSendOptions,
): string {
  const attachment = options?.attachment;
  const mentions = checkedMentions(options?.mentions);
  const quote = checkedQuote(options?.quote);
  return JSON.stringify({
    text,
    ...(attachment ? { attachment } : {}),
    ...quoteField(quote),
    ...mentionsField(mentions),
  });
}

/** Decode an inbound room-message plaintext. Tolerant: a payload without a
 * string `text` (malformed JSON, or a future shape) returns
 * `{ op: "unknown", raw }` so the caller can warn-drop it instead of throwing
 * on a single bad frame. An `attachment` whose fields don't type-check
 * drops the whole frame too (same warn-drop semantics as the Rust side,
 * where a serde failure loses the frame). */
export function decodeRoomMessage(plaintext: Uint8Array): RoomMessage {
  const raw = new TextDecoder().decode(plaintext);
  let value: {
    text?: unknown;
    attachment?: unknown;
    quote?: unknown;
    mentions?: unknown;
  };
  try {
    value = JSON.parse(raw);
  } catch {
    return { op: "unknown", raw };
  }
  if (typeof value.text === "string") {
    const att = value.attachment as FileAttachment | undefined;
    if (
      att !== undefined &&
      att !== null &&
      (typeof att !== "object" ||
        typeof att.record_id !== "string" ||
        typeof att.name !== "string" ||
        typeof att.size !== "number")
    ) {
      return { op: "unknown", raw };
    }
    const quoted = quotedSeq(value.quote);
    const named = mentionIds(value.mentions);
    return {
      op: "message",
      text: value.text,
      ...(att !== undefined && att !== null ? { attachment: att } : {}),
      ...(quoted !== undefined ? { quote: quoted } : {}),
      ...mentionsField(named),
    };
  }
  return { op: "unknown", raw };
}

/** A quoted room seq, or undefined. The wire reads `<= 0` as no quote (there
 *  is no row 0); anything that is not a positive integer is dropped rather
 *  than the whole frame -- a malformed quote must not cost the message. */
function quotedSeq(raw: unknown): number | undefined {
  return typeof raw === "number" && Number.isInteger(raw) && raw > 0
    ? raw
    : undefined;
}

/** Upper bound on a room message's mention list. The normative number is
 *  `MENTIONS_MAX` in crates/platform/kallipai-lesche-common/src/message.rs -- the
 *  Rust crate cannot be imported here, so this constant mirrors it and a test
 *  pins the value. A writer that carries more is a caller error worth naming;
 *  the decoder truncates at the same number and still decodes. */
export const MENTIONS_MAX = 16;

/** A canonical hyphenated participant id: the one form the platform writes (v5
 *  derived, v4 minted) and the one the Rust side accepts. */
const PARTICIPANT_ID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The read-side rule: deduplicate (first occurrence wins, sender order kept),
 *  cap at `MENTIONS_MAX`, and drop entries that are not participant ids. Every
 *  kept id settles on the canonical lowercase spelling. The twin of the Rust
 *  `normalize_mentions`, and private on purpose: the write side goes through
 *  `checkedMentions`, which refuses a bad shape instead of dropping it, so no
 *  caller reaches a lenient helper and routes around strictness. */
function normalizeMentions(ids: readonly string[]): string[] {
  const out: string[] = [];
  for (const raw of ids) {
    if (out.length === MENTIONS_MAX) break;
    const id = raw.toLowerCase();
    if (!PARTICIPANT_ID.test(id) || out.includes(id)) continue;
    out.push(id);
  }
  return out;
}

/** The mention ids a wire value names, or none. A value that is not a list
 *  reads as no mentions, and one bad entry costs that entry only -- the same
 *  rule the Rust decoder applies, so a crafted payload cannot cost the text. */
function mentionIds(raw: unknown): string[] {
  return Array.isArray(raw)
    ? normalizeMentions(
        raw.filter((item): item is string => typeof item === "string"),
      )
    : [];
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
  /** The room seq this line quotes, when the sender quoted one. */
  readonly quote?: number;
  /** The participant ids this line names, when the sender named any. */
  readonly mentions?: readonly string[];
  readonly createdAt: string;
  readonly mine: boolean;
  readonly failed?: boolean;
}

/** The relay-authenticated envelope sender a room line is built from. */
export interface RoomLineSender {
  readonly id: string;
  readonly kind: "human" | "agent";
  readonly handle: string;
  readonly tagma_id?: string;
}

/** Build the line one decoded payload renders as. Both inbound paths -- the
 *  history page and a live frame -- funnel through this mapping, so a payload
 *  field the surface draws is carried exactly here: a field the decoder reads
 *  but this function drops never reaches the page. `quote` is spread in only
 *  when the payload carries one, so an unquoted line carries no `quote` key
 *  at all. */
export function roomLineFrom(
  decoded: Extract<RoomMessage, { op: "message" }>,
  sender: RoomLineSender,
  options: {
    readonly seq: number;
    readonly createdAt: string;
    readonly selfId: string | null;
  },
): RoomLine {
  return {
    seq: options.seq,
    senderId: sender.id,
    senderKind: sender.kind,
    senderHandle: sender.handle,
    senderTagmaId: sender.tagma_id,
    text: decoded.text,
    ...(decoded.quote !== undefined ? { quote: decoded.quote } : {}),
    ...mentionsField(decoded.mentions),
    createdAt: options.createdAt,
    mine: sender.id === options.selfId,
  };
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
 * (`/v1/lesche/users`, `/v1/lesche/tagmata`); the two namespaces are independent. */
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
 * `crates/platform/kallipai-lesche/src/identity.rs`; the optimistic local line
 * emits the same degraded form). Splitting on the FIRST `@` is safe because of
 * two server-side invariants:
 *   - `short_prefix` is `chars().take(6)` of a server-derived participant id --
 *     no `@` (identity.rs).
 *   - an archeion username is `[a-z0-9-]` only (single interior hyphens), never
 *     `@` (`crates/platform/kallipai-archeion/src/username.rs`).
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
  // (the archeion charset is `[a-z0-9-]`, no spaces).
  return { handle: raw };
}

/** A roster member as the mention-chip resolver reads it. Structural on
 * purpose: the roster's own member type satisfies it, so the resolver stays
 * free of the wire type and names only the fields a chip draws. */
export interface MentionTarget {
  readonly id: string;
  readonly kind: "human" | "agent";
  readonly handle: string;
  readonly label?: string;
  readonly tagma_id?: string;
}

/** The short participant-id prefix -- the token a chip still has when the
 * roster cannot name the member. Char-based, matching the relay's
 * `short_prefix` (`chars().take(6)`), so a non-ASCII id cannot split a code
 * point. */
export function shortIdOf(id: string): string {
  return [...id].slice(0, 6).join("");
}

/** One mention chip as the bubble draws it, in the sender's order. The chip
 * carries three states, expressed by the pair (`member`, `unnamed`) --
 * there is no separate status field:
 *   - named: `member` holds the roster-resolved identity;
 *   - unknown: the roster landed and does not hold this id, so `unnamed` says
 *     so -- the wire carries the id alone, so the client cannot know that
 *     member's kind and must not invent one;
 *   - unloaded: the roster has not landed yet, so `unnamed` says that instead
 *     -- the client does not know yet, and must not call the member unknown.
 * A member the roster DOES hold but the registry could not resolve arrives with
 * the relay's degraded handle (`<kind> <prefix>`: `user 6f9619`, `agent
 * 6f9619`); it renders verbatim and stays unlinked -- the same rule the message
 * header follows (the human arm's handle carries no `@`, and the agent arm has
 * no `tagma_id` to link through). */
export interface MentionChip {
  readonly id: string;
  /** The viewer is the member this chip names. */
  readonly self: boolean;
  /** The roster-resolved identity, in the same tokens the header draws. */
  readonly member?: {
    readonly kind: "human" | "agent";
    readonly handle: string;
    readonly label?: string;
    readonly href?: string;
  };
  /** Present exactly when `member` is absent, and says why: `unknown` = the
   * roster landed without this id; `unloaded` = the roster has not landed yet. */
  readonly unnamed?: "unknown" | "unloaded";
}

/** Resolve a line's mentioned ids into chips. Order is the sender's, and a
 * repeated id folds to its first chip, so the list comes back unique by
 * construction -- the component can key on the id without leaning on the
 * decoder's de-duplication. `memberById` is the roster keyed by participant id,
 * `selfId` marks the viewer's own chip, and `rosterLoaded` says whether that
 * map can be trusted to be complete: while it cannot, a miss is `unloaded`
 * rather than `unknown`.
 * Pure: unit-tested in room-message_test.ts. */
export function mentionChipsOf(
  mentions: readonly string[] | undefined,
  memberById: ReadonlyMap<string, MentionTarget>,
  selfId: string | null,
  rosterLoaded: boolean,
): MentionChip[] {
  if (!mentions || mentions.length === 0) return [];
  const seen = new Set<string>();
  const chips: MentionChip[] = [];
  for (const id of mentions) {
    if (seen.has(id)) continue;
    seen.add(id);
    const self = selfId !== null && id === selfId;
    const target = memberById.get(id);
    if (!target) {
      chips.push({ id, self, unnamed: rosterLoaded ? "unknown" : "unloaded" });
      continue;
    }
    const href = profileHref(target.kind, target.handle, target.tagma_id);
    chips.push({
      id,
      self,
      member: {
        kind: target.kind,
        handle: target.handle,
        ...(target.label !== undefined ? { label: target.label } : {}),
        ...(href !== undefined ? { href } : {}),
      },
    });
  }
  return chips;
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

/** Whether a transcript line renders as still-sending (the dimmed pending
 * bubble). TRUE only for the caller's OWN optimistic lines: they carry a
 * synthetic negative seq until the next history fetch lands the
 * lesche-assigned seq. A live INBOUND frame carries a synthetic seq too (the
 * SSE envelope has no server seq), but an inbound message is by definition
 * already delivered -- reading its missing seq as "pending" is what left
 * received room messages permanently grey. The view must ask mine, not the
 * seq sign alone. Pure: unit-tested in room-message_test.ts. */
export function roomLinePending(line: Pick<RoomLine, "mine" | "seq">): boolean {
  return line.mine && line.seq < 0;
}

/** The anchor id a room row stamps on itself, and the id a quote block points
 *  at. Keyed by the room seq -- the transcript's own numbering -- and
 *  namespaced so it cannot collide with the bilateral transcript's ids. Pure:
 *  unit-tested in room-message_test.ts. */
export function roomRowAnchorId(seq: number): string {
  return "room-msg-a11y-" + seq;
}

/** Who wrote a room row, as spoken: the roster's display name once it has
 *  landed, then the sender's handle, then the standing name for its kind.
 *  Shared by the row's label and by the jump announcement, so a row reads
 *  the same way in both. Pure: unit-tested in room-message_test.ts. */
export function roomAuthor(
  line: Pick<RoomLine, "senderHandle" | "senderKind">,
  authorLabel?: string,
): string {
  const rosterLabel = authorLabel ?? "";
  if (rosterLabel !== "") return rosterLabel;
  // Below the roster label both surfaces' author chains land on one step.
  return authorName(
    line.senderHandle,
    line.senderKind === "agent" ? sender_agent_aria() : sender_user_aria(),
  );
}

/** What a room row says about itself to a screen reader: who wrote it, when,
 *  and whether it answers another message. The room transcript stamps every
 *  line with its own createdAt and keys rows by seq, so both parts are always
 *  at hand (the bilateral view reads its time off the timeline markers
 *  instead). `time` is pre-rendered by the caller, which owns the timezone
 *  setting; `authorLabel` is the roster's display name once the roster has
 *  landed. Pure: unit-tested in room-message_test.ts. */
export function roomMessageLabel(
  line: Pick<RoomLine, "senderHandle" | "senderKind" | "quote">,
  parts: { readonly time: string; readonly authorLabel?: string },
): string {
  const author = roomAuthor(line, parts.authorLabel);
  return line.quote !== undefined
    ? chat_message_label_quoting_timed({ author, time: parts.time })
    : chat_message_label_timed({ author, time: parts.time });
}

/** The sentence a jump to a room row speaks: who wrote it, and when. The
 *  author reads through the same chain the row's own label reads, so a jump
 *  never calls the sender something the transcript shows it as. `time` is
 *  pre-rendered by the caller, which owns the timezone setting. Pure:
 *  unit-tested in room-message_test.ts. */
export function roomRowAnnouncement(
  line: Pick<RoomLine, "senderHandle" | "senderKind">,
  parts: { readonly time?: string; readonly authorLabel?: string },
): string {
  return jumpAnnouncement(roomAuthor(line, parts.authorLabel), parts.time);
}
