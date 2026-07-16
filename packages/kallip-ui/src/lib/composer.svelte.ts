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
}

export interface ComposerModel {
  /** Two-way bindable draft text. */
  draft: string;
  /** Increments on each requestFocus; an effect keyed on it focuses the field. */
  readonly focusToken: number;
  readonly canSend: boolean;
  requestFocus: () => void;
  submit: () => Promise<void>;
}

export function createComposer(options: ComposerOptions): ComposerModel {
  let draft = $state("");
  let focusToken = $state(0);

  return {
    get draft() {
      return draft;
    },
    set draft(value: string) {
      draft = value;
    },
    get focusToken() {
      return focusToken;
    },
    get canSend() {
      return options.canSubmit() && draft.trim().length > 0;
    },
    requestFocus() {
      focusToken += 1;
    },
    async submit() {
      const value = draft.trim();
      if (!value || !options.canSubmit()) return;
      draft = "";
      await options.send(value);
    },
  };
}
