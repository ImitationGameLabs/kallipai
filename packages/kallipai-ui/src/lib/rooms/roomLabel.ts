/** A room's display name, shared by the room pages and the shell trail: the
 *  operator-set name when the registry has one, an id-prefixed fallback while
 *  it is still resolving, so both surfaces name a room the same way. */
import { room_label_fallback } from "../../paraglide/messages.js";

/** The rooms that carry a name: the registry's rows, or the one room a page
 *  already resolved. */
export interface NamedRoom {
  readonly room_id: string;
  readonly name: string;
}

export function roomLabelOf(
  rooms: readonly NamedRoom[],
  roomId: string,
): string {
  const name = rooms.find((room) => room.room_id === roomId)?.name;
  return name || room_label_fallback({ id: roomId.slice(0, 8) });
}
