<script lang="ts">
  import { toneDot, type NarrationTone } from "../../lib/manage/turnView.ts";
  // One line of narration above a call's body: a state dot and a sentence set
  // in the page's own font. The dot carries the state colour and the sentence
  // keeps the text colour, so a long path after "Failed" does not shout.
  //
  // A sentence carrying a value the reader is meant to copy, such as an
  // approval id, sets whole in the record's font instead: there copying
  // beats the narration voice. The font goes on the sentence rather than the
  // value alone, since a switch mid-string breaks an i18n sentence in two.
  //
  // Every tone paints with a paired shade, so each dot keeps a floor against
  // the card (bg-surface-100-900) in both modes; the pairs live in toneDot.
  //
  // Which paint a tone gets is toneDot's call; this component only puts the
  // dot in its column and the sentence beside it. The sentence states the
  // outcome, so the dot never has to carry it alone.
  //
  // The dot sits in a fixed 8px column, so the sentence starts at the same x
  // in every view whether or not a dot paints.
  // A line under the narration starts at that same x, so it carries pl-3.5:
  // the dot is a prefix, not part of the indent a reader follows.
  let {
    tone,
    text,
    mono = false,
  }: {
    /** The outcome the caller read from the call; toneDot turns it into a
     * shade. */
    tone: NarrationTone;
    text: string;
    /** Set when the sentence carries a value the reader may need to copy,
     * such as an id: it then sets in the record's font. */
    mono?: boolean;
  } = $props();

  const dot = $derived(toneDot(tone));
</script>

<p class="flex items-start gap-1.5 text-[13px]">
  <span class="mt-1 flex w-2 shrink-0 justify-center">
    <span class="size-2 rounded-full {dot}"></span>
  </span>
  <span class="min-w-0 break-words {mono ? 'font-mono' : ''}">{text}</span>
</p>
