// What a transcript row is called, and what a jump to it says. Both chat
// surfaces jump through the same reveal primitive and differ only in which row
// table they resolve a row from, so the wording lives here once and each
// page's `describe` closure supplies the row it found.

import {
  chat_quote_jump_announcement,
  chat_quote_jump_announcement_untimed,
} from "../paraglide/messages.js";

/** The sentence a jump speaks: who wrote the row it landed on, and when. A row
 *  whose send time is not available (a cached line written before the field
 *  existed, a system line with none) speaks the author alone rather than
 *  dangling a time that never comes. */
export function jumpAnnouncement(author: string, time?: string): string {
  if (time === undefined) {
    return chat_quote_jump_announcement_untimed({ author });
  }
  return chat_quote_jump_announcement({ author, time });
}

/** How a line's author is called: their handle when the line carries one, else
 *  the standing name for their kind. Shared by both surfaces' author chains;
 *  each still reads its own row shape above this step. */
export function authorName(
  handle: string | undefined,
  standing: string,
): string {
  return handle !== undefined && handle !== "" ? handle : standing;
}
