// Decision-table tests for the dangling-save flow and the unsaved
// leave guard. The store's catch block and the host wrapper are thin
// layers over classifySaveFailure / leaveGuardIntercept /
// leaveGuardDialogVisible — these tests pin the decisions directly.

import { assertEquals } from "@std/assert";
import { KallipaiError } from "@kallipai/kallipai-common";
import {
  classifySaveFailure,
  leaveGuardDialogVisible,
  leaveGuardIntercept,
} from "./profiles-view.ts";

function dangling409(): KallipaiError {
  return new KallipaiError({
    status: 409,
    message: "config drops sets still bound by agents",
    dangling: ["agent-x → 'alt'"],
  });
}

Deno.test("a structured 409 parks the stranded list", () => {
  assertEquals(classifySaveFailure(dangling409(), false), "park-dangling");
  assertEquals(classifySaveFailure(dangling409(), true), "park-dangling");
});

Deno.test("a bare 409 under force means an old backend", () => {
  const bare = new KallipaiError({
    status: 409,
    message: "config drops sets still bound by agents",
  });
  assertEquals(classifySaveFailure(bare, true), "stale-backend");
  // Without force this is just a normal conflict rejection.
  assertEquals(classifySaveFailure(bare, false), "error");
});

Deno.test("non-conflict failures stay generic errors", () => {
  assertEquals(classifySaveFailure(new Error("network down"), true), "error");
  assertEquals(
    classifySaveFailure(
      new KallipaiError({ status: 400, message: "bad" }),
      true,
    ),
    "error",
  );
});

Deno.test("the guard intercepts only a dirty, not-yet-open state", () => {
  assertEquals(leaveGuardIntercept(true, false), true);
  assertEquals(leaveGuardIntercept(false, false), false);
  assertEquals(leaveGuardIntercept(true, true), false);
});

Deno.test("the guard dialog waits while the dangling confirm is up", () => {
  assertEquals(leaveGuardDialogVisible(true, null), true);
  assertEquals(leaveGuardDialogVisible(true, ["agent-x → 'alt'"]), false);
  assertEquals(leaveGuardDialogVisible(false, null), false);
});
