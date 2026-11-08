// A write refused by the conversation itself, before any carrier sees it: the
// conversation is closed, or no carrier is attached to take the line.
//
// Separate from lib/errors.ts on purpose -- that module renders paraglide
// messages, and the session core must raise a refusal without depending on the
// i18n layer. `classifyError` imports this way round instead.

export type WriteRefusalReason = "closed" | "carrier-absent";

export class WriteRefusal extends Error {
  constructor(reason: WriteRefusalReason) {
    // Console-facing only: classifyError renders a message of its own, so this
    // string never reaches the user.
    super(reason === "closed" ? "conversation closed" : "not connected");
    this.name = "WriteRefusal";
  }
}

/** Name check as well as instanceof: a bundler that duplicates this module
 *  would otherwise break the fallback `classifyError` relies on. */
export function isWriteRefusal(e: unknown): e is WriteRefusal {
  return (
    e instanceof WriteRefusal ||
    (e instanceof Error && e.name === "WriteRefusal")
  );
}
