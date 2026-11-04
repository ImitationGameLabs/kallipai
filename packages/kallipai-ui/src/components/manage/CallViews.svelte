<script module lang="ts">
  // The tools this build draws itself, one line each. A tool enters only when
  // its own view exists: the shape chain below is the layout for everything
  // else, and this table is the one switch that decides between them.
  //
  // The table sits in a component module rather than a plain `.ts` one
  // because `deno test` imports `toolViews.ts` and the Deno side has no
  // Svelte compiler. The validators and the picking stay there; this file
  // only wires names to components.
  import CallTerminal from "./CallTerminal.svelte";
  import CallBreak from "./CallBreak.svelte";
  import CallBgRead from "./CallBgRead.svelte";
  import CallEvict from "./CallEvict.svelte";
  import CallContextOp from "./CallContextOp.svelte";
  import {
    isBashExecResult,
    isBreakResult,
    isBgReadResult,
    isEvictResult,
    isContextPinResult,
    isContextUnpinResult,
    pickView,
    view,
    type ParsedEnvelope,
    type ToolView,
    type ViewPick,
  } from "../../lib/manage/toolViews.ts";

  export const TOOL_VIEWS: Record<string, ToolView> = {
    bash_exec: view(isBashExecResult, CallTerminal),
    break: view(isBreakResult, CallBreak),
    bash_background_read: view(isBgReadResult, CallBgRead),
    context_evict: view(isEvictResult, CallEvict),
    context_pin: view(isContextPinResult, CallContextOp),
    context_unpin: view(isContextUnpinResult, CallContextOp),
  };

  /** The registry-bound picker: one table, one call site. */
  export function pickRegisteredView(
    name: string,
    parsed: ParsedEnvelope,
  ): ViewPick {
    return pickView(name, parsed, TOOL_VIEWS);
  }
</script>
