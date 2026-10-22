// Session drafts: per-conversation composer draft text that survives page
// switches within the SPA and a same-tab reload (sessionStorage mirror), but
// never persists across tabs or browser restarts. Drafts are user content in
// flight, not cached data. The store is a plain Map with no reactivity -- its
// only reader is bindDraft's restore effect (drafts.svelte.ts), which pulls
// once per key change -- which keeps this module runnable from plain Deno
// tests without the Svelte compiler.

import type { DraftBinding } from "../composer.svelte.ts";

/** `sessionStorage` keys holding drafts share this prefix; `reset` scans it. */
const PREFIX = "kallipai.draft.";

/** The storage surface the drafts store mirrors into. Narrowed from the full
 * `Storage` interface so tests stub it with a Map and the default binding can
 * degrade to memory when sessionStorage is unavailable. */
export interface DraftStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
  /** All held keys (snapshot), for `reset`'s prefix scan. */
  keys(): string[];
}

/** A no-op storage: drafts then live only in the in-memory map for this page
 * load (still surviving page switches, losing the reload case). */
function memoryStorage(): DraftStorage {
  const data = new Map<string, string>();
  return {
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => void data.set(k, v),
    removeItem: (k) => void data.delete(k),
    keys: () => [...data.keys()],
  };
}

/** sessionStorage behind the DraftStorage shape, degraded to memory when
 * access throws or is undefined (SSR pass, sandboxed frame, privacy mode).
 * `typeof` guards the global so importing this module under Deno is safe. */
function sessionStorageOrFail(): DraftStorage {
  try {
    const s =
      typeof sessionStorage === "undefined" ? undefined : sessionStorage;
    if (!s) return memoryStorage();
    return {
      getItem: (k) => s.getItem(k),
      setItem: (k, v) => s.setItem(k, v),
      removeItem: (k) => s.removeItem(k),
      keys: () => {
        const out: string[] = [];
        for (let i = 0; i < s.length; i++) out.push(s.key(i)!);
        return out;
      },
    };
  } catch {
    return memoryStorage();
  }
}

export class ChatDraftsStore {
  private drafts = new Map<string, string>();
  private readonly storage: DraftStorage;

  constructor(storage: DraftStorage = sessionStorageOrFail()) {
    this.storage = storage;
    // Seed from storage so a reload restores drafts without re-typing. Only
    // prefixed keys are adopted; everything else in storage is foreign.
    for (const held of storage.keys()) {
      if (held.startsWith(PREFIX)) {
        this.drafts.set(held.slice(PREFIX.length), storage.getItem(held)!);
      }
    }
  }

  get(key: string): string {
    return this.drafts.get(key) ?? "";
  }

  /** Store `text` under `key`, mirrored into storage. Empty text DELETES the
   * entry: a cleared or submitted draft leaves no residue in either layer. */
  set(key: string, text: string): void {
    if (text === "") {
      this.drafts.delete(key);
      this.storage.removeItem(PREFIX + key);
      return;
    }
    this.drafts.set(key, text);
    this.storage.setItem(PREFIX + key, text);
  }

  /** Drop every draft (logout): drafts are user content and must not leak
   * into the next session on a shared device. Only prefixed keys are
   * removed; foreign storage entries are left alone. */
  reset(): void {
    this.drafts.clear();
    for (const held of this.storage.keys()) {
      if (held.startsWith(PREFIX)) this.storage.removeItem(held);
    }
  }
}

export const chatDraftsStore = new ChatDraftsStore();

/** Where a failed send puts its text back. The store entry always returns
 *  under the key the submit started with -- that is the conversation the
 *  reader typed in -- while the visible field is repainted only while the
 *  composer still holds the very binding the submit captured. Identity, not
 *  key equality: a return to the same conversation installs a new binding. */
export function restoreFailedDraft(args: {
  /** Key the submit started under: where the text is stored. */
  readonly sendingKey: string;
  /** The binding the submit captured; the restore runs from its own
   *  closure, so the composer's binding is always present here. */
  readonly sendingBinding: DraftBinding;
  /** The binding installed now; undefined when nothing is bound. */
  readonly liveBinding: DraftBinding | undefined;
  readonly text: string;
  readonly store: Pick<ChatDraftsStore, "set">;
  /** Show the text in the live field (the composer's draft setter). */
  readonly show: (text: string) => void;
}): void {
  args.store.set(args.sendingKey, args.text);
  // The key only decides where the text is stored; the binding decides
  // whether it is painted.
  if (args.liveBinding === args.sendingBinding) {
    args.show(args.text);
  }
}

// Stable draft keys per conversation surface. The bilateral tagma chat is
// keyed on the tagma id -- stable across re-KEX and shared by both entries
// into the page (the sidebar /tagma/{id}/chat route and a /chat/{conversationId}
// deep link resolve to the same Conversation, hence the same key). Rooms key
// on the room id. Anything else (the local conversation, or a relay id in
// the brief window before the Conversation resolves) falls back to the
// conversation id itself, so a typed draft never lands on a shared bucket.
export const tagmaDraftKey = (tagmaId: string) => `t:${tagmaId}`;
export const roomDraftKey = (roomId: string) => `r:${roomId}`;
export const convDraftKey = (conversationId: string) => `c:${conversationId}`;

/** The one storage key a chat page derives and shares: its draft and its
 *  attachment tray then key on the same bucket. The tagma id once the
 *  conversation resolved to a relay channel (stable across re-KEX, shared by
 *  both routes into the page); the conversation id otherwise -- the local
 *  conversation, or a relay id in the brief window before it resolves. */
export function conversationStorageKey(
  conversationId: string,
  relayTagmaId: string | undefined,
): string {
  return relayTagmaId === undefined
    ? convDraftKey(conversationId)
    : tagmaDraftKey(relayTagmaId);
}
