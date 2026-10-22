// The attachment tray: one bucket of picked files per conversation key, held in
// memory for the life of the page. Upload orchestration still lives with the
// page (it owns the files client); the tray owns only *whose* items these are.
//
// Three rules, and they are the whole reason for the key:
//
// A write that a late answer will make names the key it was started under.
// add and patch carry `owner`, captured when the work began -- an upload
// started in one conversation can finish after the reader moved on, and the
// minted record id is addressed to a tagma, so writing it into whatever tray
// is on screen would let the next send deliver a file to the wrong recipient.
//
// A write the reader's gesture causes -- remove, clear -- acts on the
// conversation on screen, and the caller passes that key as well: the list
// the gesture came from is the screen's list.
//
// Reads are the only place the live key is consulted: current() shows the
// bucket of the conversation the page is on.
//
// Anything async added later takes its key before the first await, the way
// add and patch do; the live key is never a stand-in for it.

import type { AttachmentItem } from "./attachments.ts";

export interface AttachmentTray {
  /** The items held for the current conversation key (empty without one). */
  current(): AttachmentItem[];
  /** Append to `owner`'s bucket (the key captured when the file was picked). */
  add(owner: string, item: AttachmentItem): void;
  /** Patch one item inside `owner`'s bucket only. */
  patch(owner: string, id: string, patch: Partial<AttachmentItem>): void;
  /** Drop one item from `owner`'s bucket (the conversation on screen). */
  remove(owner: string, id: string): void;
  /** Drop every item in `owner`'s bucket (after a send). */
  clear(owner: string): void;
}

export function createAttachmentTray(key: () => string): AttachmentTray {
  let trays = $state<Record<string, AttachmentItem[]>>({});

  const read = (owner: string): AttachmentItem[] => trays[owner] ?? [];
  const write = (owner: string, items: AttachmentItem[]): void => {
    trays = { ...trays, [owner]: items };
  };

  return {
    current: () => read(key()),
    add: (owner, item) => write(owner, [...read(owner), item]),
    patch: (owner, id, patch) => {
      write(
        owner,
        read(owner).map((a) => (a.id === id ? { ...a, ...patch } : a)),
      );
    },
    remove: (owner, id) => {
      write(
        owner,
        read(owner).filter((a) => a.id !== id),
      );
    },
    clear: (owner) => write(owner, []),
  };
}
