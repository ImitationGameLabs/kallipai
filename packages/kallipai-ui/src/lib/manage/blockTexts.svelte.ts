// Owner of the stored text of each call block on the turn page, and the only
// place that writes it. A read is opened with `begin`, which marks the block
// as filling and hands back the token naming that read; the answer arrives at
// `settle`, which writes only when the block still carries that token. Keeping
// both writes here is what makes a read that never marked its block
// unrepresentable: there is no other way in, and an answer that comes back
// after a newer read, another turn or another agent writes nothing at all.
//
// The getter hands out the map itself; what the page draws from are the
// entries in it, and those are frozen as they are written, so a page that
// grabs one cannot change what the next page reads.
//
// One instance belongs to one page. Two pages sharing one would let a read
// opened on one land in a block the other is showing, and the other stores
// in this layer are module singletons, so the arrangement is worth stating.
import { bodyStillMine, finishRead, startRead } from "./turnView.ts";
import type { BodyEntry, BodyOutcome } from "./turnView.ts";

export class BlockTexts {
  private entriesState = $state<Record<string, BodyEntry>>({});
  /** Names the read that opened the newest block; only ever grows, so a
   * token from an older era cannot come back around. */
  private token = 0;

  /** What the page draws from. */
  get entries(): Readonly<Record<string, BodyEntry>> {
    return this.entriesState;
  }

  /** Open a read on this key: the block shows that a read is in flight while
   * the ranges already read stay on screen. Returns the token to settle it
   * with; nothing else can write the block. */
  begin(key: string): number {
    this.token += 1;
    const token = this.token;
    this.entriesState = {
      ...this.entriesState,
      [key]: Object.freeze(startRead(this.entriesState[key] ?? null, token)),
    };
    return token;
  }

  /** Hand an answer to the block it belongs to. A block that has moved on
   * (a newer read, another turn, a cleared page) keeps what it shows. The
   * token answers one read: settling the same one again writes again, and
   * nothing here calls it twice. */
  settle(key: string, token: number, outcome: BodyOutcome): void {
    const entry = this.entriesState[key];
    if (!bodyStillMine(entry, token)) return;
    this.entriesState = {
      ...this.entriesState,
      [key]: Object.freeze(finishRead(entry, outcome)),
    };
  }

  /** Drop every block and turn every read still in flight into a no-op, for
   * a page that now shows another agent or another turn. */
  clear(): void {
    this.entriesState = {};
    this.token += 1;
  }
}
