// Shape the flat-field tool payloads into FieldList rows + chip lists. Kept as
// a pure function so ToolResultCard stays a clean name -> component registry and
// the per-tool shaping is centralized and testable. Tools with nested/complex
// payloads (e.g. exec_policy) are intentionally NOT shaped here — they fall
// through to the generic pretty-JSON renderer so no data is hidden.

export interface ShapedFields {
  title?: string;
  rows: { label: string; detail?: string | number }[];
  labels?: { label: string }[];
}

interface LabelsLike {
  pinned?: string;
  unpinned?: string;
  source?: string;
  pinned_labels?: string[];
  task_id?: string;
}

function labelItems(
  labels: string[] | undefined,
): { label: string }[] | undefined {
  return labels?.map((label) => ({ label }));
}

// Returns the shaped fields for a known flat-field tool, or null to signal
// "no dedicated shape -> use generic".
export function shapeFields(
  toolName: string,
  result: unknown,
): ShapedFields | null {
  const r = (result ?? {}) as LabelsLike;
  switch (toolName) {
    case "context_pin":
      return {
        rows: [{ label: "pinned", detail: r.pinned ?? "" }],
        labels: labelItems(r.pinned_labels),
      };
    case "context_unpin":
      return {
        rows: [{ label: "unpinned", detail: r.unpinned ?? "" }],
        labels: labelItems(r.pinned_labels),
      };
    case "bash_background_kill":
      return { rows: [{ label: "killed task", detail: r.task_id ?? "" }] };
    default:
      return null;
  }
}
