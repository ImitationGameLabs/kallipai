// The surface rule as behavior: the transcript's own failure, the link's
// classified copy, whether a surface above already states that cause, and what
// the composer would say. The link's copy on the line silences the note; the
// transcript's own error line leaves the note alone, since the two answer
// different questions.
import { assertEquals } from "@std/assert";
import { errorSurface, type ErrorSurfaceInput } from "./errorSurface.ts";

const COPY = "Couldn't reach the tagma";

function surface(over: Partial<ErrorSurfaceInput>): ErrorSurfaceInput {
  return {
    transcriptStatus: "idle",
    linkCopy: null,
    causeSpokenAbove: false,
    notice: null,
    ...over,
  };
}

Deno.test("a link failure fills the line and silences the notice", () => {
  const out = errorSurface(
    surface({ linkCopy: COPY, notice: "not-connected" }),
  );
  assertEquals(out.error, COPY);
  assertEquals(out.status, "error");
  assertEquals(out.notice, null);
});

Deno.test("a cause stated above keeps the page's line empty", () => {
  const out = errorSurface(
    surface({
      linkCopy: COPY,
      causeSpokenAbove: true,
      notice: "not-connected",
    }),
  );
  assertEquals(out.error, undefined);
  assertEquals(out.status, "idle");
  assertEquals(out.notice, "not-connected");
});

Deno.test("a transcript error leaves the composer's note in place", () => {
  const out = errorSurface(
    surface({
      transcriptError: "The message was rejected",
      transcriptStatus: "error",
      notice: "not-connected",
    }),
  );
  assertEquals(out.error, "The message was rejected");
  assertEquals(out.notice, "not-connected");
});

Deno.test("the transcript's own error wins over the link copy", () => {
  const out = errorSurface(
    surface({
      transcriptError: "The message was rejected",
      transcriptStatus: "error",
      linkCopy: COPY,
      notice: "not-connected",
    }),
  );
  assertEquals(out.error, "The message was rejected");
  assertEquals(out.status, "error");
  assertEquals(out.notice, null);
});

Deno.test("an empty transcript error leaves the line to the link copy", () => {
  const out = errorSurface(
    surface({ transcriptError: "", linkCopy: COPY, notice: "not-connected" }),
  );
  assertEquals(out.error, COPY);
  assertEquals(out.status, "error");
  assertEquals(out.notice, null);
});

Deno.test("a quiet surface passes the status through untouched", () => {
  for (const status of ["idle", "busy"] as const) {
    const out = errorSurface(surface({ transcriptStatus: status }));
    assertEquals(out.error, undefined, status);
    assertEquals(out.status, status);
    assertEquals(out.notice, null);
  }
});
