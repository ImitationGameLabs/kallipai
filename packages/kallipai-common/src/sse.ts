// Generic text/event-stream parser shared by both transports. The tagma's
// authenticated SSE endpoint cannot use EventSource (it cannot set an
// Authorization header), so both clients fetch the stream and parse the framing
// here. Each transport then decodes its own raw event shape from the `data`
// string.
//
// Handles chunk-split data lines, multi-event chunks, multi-line `data:` values
// (joined with "\n"), and ":comment"/keepalive lines (which produce no event).

export interface RawSseEvent {
  /** The joined `data:` payload (multiple data lines concatenated with "\n"). */
  readonly data: string;
  /** The `event:` field, if present. */
  readonly event?: string;
  /** The `id:` field, if present. */
  readonly id?: string;
}

/**
 * Yield parsed SSE events from a fetch Response body until the stream ends.
 * Throws if the response has no body.
 */
export async function* parseSseStream(
  response: Response,
  signal?: AbortSignal,
): AsyncGenerator<RawSseEvent> {
  if (!response.body) {
    throw new Error("SSE response has no body");
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  const onAbort = () => reader.cancel().catch(() => {});
  signal?.addEventListener("abort", onAbort);

  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      let boundary = matchBlankLine(buffer);
      while (boundary !== null) {
        const block = buffer.slice(0, boundary.index);
        buffer = buffer.slice(boundary.index + boundary.length);
        const ev = parseBlock(block);
        if (ev) yield ev;
        boundary = matchBlankLine(buffer);
      }
    }
    // Flush a trailing event without a final blank line.
    const tail = parseBlock(buffer);
    if (tail) yield tail;
  } finally {
    signal?.removeEventListener("abort", onAbort);
    reader.releaseLock();
  }
}

interface Boundary {
  readonly index: number;
  readonly length: number;
}

// Locate the next blank-line separator (\r\n\r\n | \n\n | \r\r) in the buffer.
function matchBlankLine(input: string): Boundary | null {
  const re = /\r\n\r\n|\n\n|\r\r/;
  const m = re.exec(input);
  if (!m) return null;
  const length = m[0]?.length ?? 0;
  return { index: m.index, length };
}

// Parse one event block (the text between blank lines) into a RawSseEvent, or
// null if the block carries no data (comment / keepalive).
function parseBlock(block: string): RawSseEvent | null {
  const data: string[] = [];
  let event: string | undefined;
  let id: string | undefined;

  for (const line of block.split(/\r\n|\n|\r/)) {
    if (line === "" || line.startsWith(":")) continue;
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    switch (field) {
      case "data":
        data.push(value);
        break;
      case "event":
        event = value;
        break;
      case "id":
        id = value;
        break;
      default:
        // Ignore unknown fields (retry, etc.).
        break;
    }
  }

  if (data.length === 0) return null;
  return { data: data.join("\n"), event, id };
}
