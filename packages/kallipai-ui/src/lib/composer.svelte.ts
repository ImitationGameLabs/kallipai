// Headless composer state-machine. Owns the draft text and a focus-request
// token so an external trigger (e.g. an empty-state prompt chip) can fill the
// draft and focus the textarea. Submission is delegated via an injected
// `send` callback so the module stays free of any session/store import and
// remains reusable across consuming apps.

export interface ComposerOptions {
  /** Submit the (already trimmed) prompt. */
  readonly send: (text: string) => void | Promise<void>;
  /** Whether submission is currently permitted (connected, not busy). */
  readonly canSubmit: () => boolean;
  /** Allow submission with an empty draft (an attachment-only send,
   * gated on all uploads being ready by the page's predicate). Omitted,
   * the non-empty draft rule holds. */
  readonly allowEmpty?: () => boolean;
}

/** The draft a composer is currently bound to: the conversation key plus the
 *  writer that puts text back for it. bindDraft installs a fresh object per
 *  key change, so a submit compares objects, not key strings. */
export interface DraftBinding {
  readonly key: string;
  /** Put `text` back for this key; repaints the field only if still bound. */
  restore: (text: string) => void;
}

export interface ComposerModel {
  /** Two-way bindable draft text. */
  draft: string;
  /** The binding bindDraft installed for the current conversation key. A
   *  submit captures it up front and compares the object, not the key; a
   *  composer with no binding refuses to send, with a logged contract error. */
  binding: DraftBinding | undefined;
  /** Increments on each requestFocus; an effect keyed on it focuses the field. */
  readonly focusToken: number;
  /** The caret the field last reported (textarea selectionStart). The mention
   *  picker reads the draft through it; without it a draft is just text and an
   *  `@` the user has already typed past would still open a popup. */
  caret: number;
  /** Increments on each requestCaret; an effect keyed on it places the caret. */
  readonly caretToken: number;
  /** Where requestCaret put the caret (the last request wins). */
  readonly caretRequest: number | undefined;
  readonly canSend: boolean;
  /** True while the conversation this composer is bound to has a submit in
   *  flight (covers the parked retry window); drives the button spinner.
   *  Another conversation's in-flight send leaves this one free. */
  readonly sending: boolean;
  requestFocus: () => void;
  /** Move the caret to `position` after the next flush -- accepting a
   *  candidate rewrites the draft, and a rewritten field puts the caret at the
   *  end; this puts it back where the token was. */
  requestCaret: (position: number) => void;
  submit: () => Promise<void>;
}

export function createComposer(options: ComposerOptions): ComposerModel {
  let draft = $state("");
  /** Keys with a submit in flight, so one conversation's POST cannot
   *  disable another conversation's send button. */
  let sendingKeys = $state<Record<string, true>>({});
  let focusToken = $state(0);
  let caret = $state(0);
  let caretToken = $state(0);
  let caretRequest = $state<number | undefined>(undefined);
  /** The binding this composer holds. As plain state it would be proxied
   *  on every read, and the failed-send path compares bindings by identity;
   *  $state.raw tracks the reads without wrapping the object, so a rebind
   *  reaches the button and the spinner while that identity stays intact.
   *  It wakes readers on assignment alone: assign it, never mutate in place. */
  let binding = $state.raw<DraftBinding | undefined>(undefined);
  /** Whether this composer's own conversation has a submit in flight. */
  const inFlight = (): boolean =>
    binding !== undefined && sendingKeys[binding.key] === true;

  return {
    get draft() {
      return draft;
    },
    set draft(value: string) {
      draft = value;
    },
    get binding() {
      return binding;
    },
    set binding(value: DraftBinding | undefined) {
      binding = value;
    },
    get focusToken() {
      return focusToken;
    },
    get caret() {
      return caret;
    },
    set caret(value: number) {
      caret = value;
    },
    get caretToken() {
      return caretToken;
    },
    get caretRequest() {
      return caretRequest;
    },
    get sending() {
      return inFlight();
    },
    get canSend() {
      return (
        options.canSubmit() &&
        (draft.trim().length > 0 || (options.allowEmpty?.() ?? false))
      );
    },
    requestFocus() {
      focusToken += 1;
    },
    requestCaret(position: number) {
      caretRequest = position;
      caretToken += 1;
    },
    async submit() {
      const value = draft.trim();
      // Hold the binding this submit started under: the reader may move to
      // another conversation before the transport answers.
      const bound = binding;
      // allowEmpty lifts the non-empty rule (attachment-only sends once
      // every upload is ready); the default keeps the historical gate.
      const emptyOk = options.allowEmpty?.() ?? false;
      if ((!value && !emptyOk) || !options.canSubmit() || inFlight()) return;
      if (bound === undefined) {
        // A host that never installed a binding cannot say where this text
        // came from, so a failed send would have nowhere safe to put it
        // back. Sending anyway is the cross-conversation write this seam
        // exists to prevent: refuse, and make the broken contract loud.
        console.error("composer submit without a draft binding");
        return;
      }
      const sendingKey = bound.key;
      sendingKeys = { ...sendingKeys, [sendingKey]: true };
      draft = "";
      try {
        await options.send(value);
      } catch (e) {
        // Restore the text where it was typed. Through the captured binding
        // it always lands under the sending conversation's key, and the field
        // is repainted only while the composer still holds this same binding.
        // The caller surfaces the failure: a room store marks the optimistic
        // line failed and the page renders its retry.
        bound.restore(value);
        if (e instanceof Error) console.error("send failed:", e.message);
      } finally {
        const next = { ...sendingKeys };
        delete next[sendingKey];
        sendingKeys = next;
      }
    },
  };
}
