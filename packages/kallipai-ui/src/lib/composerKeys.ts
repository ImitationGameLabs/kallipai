// The composer textarea's keystroke vocabulary, kept out of the component so
// the IME rule is testable without mounting one: a key event that arrives
// while an input method is composing belongs to the IME (Enter confirms a
// candidate, Escape cancels it), never to the chat surface.
//
// The layout half is the desktop convention: at md+ Enter submits and
// Shift+Enter breaks the line; below md Enter is a newline and the send button
// is the only submit (mobile keyboards pair Enter with a newline habit, so
// submit-on-Enter mistypes).

/** What one key event means to the composer. */
export type ComposerKeyIntent = "submit" | "cancel-quote" | "none";

/** The key fields the decision reads; a KeyboardEvent satisfies it. */
export interface ComposerKey {
  readonly key: string;
  readonly shiftKey: boolean;
  readonly isComposing: boolean;
}

export function composerKeyIntent(
  event: ComposerKey,
  desktop: boolean,
): ComposerKeyIntent {
  if (event.isComposing) return "none";
  if (event.key === "Escape") return "cancel-quote";
  if (desktop && event.key === "Enter" && !event.shiftKey) return "submit";
  return "none";
}
