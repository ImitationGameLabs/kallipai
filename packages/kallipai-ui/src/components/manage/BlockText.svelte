<script lang="ts">
  // One stretch of stored text. This is the only place a text becomes a
  // `<pre>`, and `kind` is the one fact the box takes from its caller: whether
  // a machine wrote the text or a person did. Machine output reads as a
  // terminal: monospace, a surface behind it, a ring, and a fixed height it
  // scrolls inside, since such output is routinely long. A message or a
  // reasoning block is prose: wrapped the same way, and otherwise bare, so a
  // paragraph runs down the page instead of scrolling in a well.
  // Not folding: whether more text exists is the record's answer, which the
  // paged reads carry, and a line count is no such answer.
  //
  // `failed` marks the box, never the text: a failed command's output is still
  // what the command said.

  let {
    text,
    kind = "machine",
    failed = false,
  }: {
    /** The text as stored. */
    text: string;
    /** Who wrote it: a tool's output, or a message meant for a person. */
    kind?: "machine" | "prose";
    /** A left edge in the error colour. */
    failed?: boolean;
  } = $props();

  const shell = $derived(
    kind === "machine"
      ? "max-h-96 overflow-auto rounded bg-surface-50-950 p-2 font-mono ring-1 ring-surface-200-800"
      : "",
  );
  const edge = $derived(failed ? "border-l-2 border-error-700-300" : "");
</script>

<pre class="whitespace-pre-wrap break-words text-xs {shell} {edge}">{text}</pre>
