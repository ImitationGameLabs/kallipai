// Source-read pins for the files page surface (same rationale as
// message_bubble_test: the wiring is presentation contracts a typecheck
// cannot see). The dashboard owns the I/O; the row stays presentational;
// the chat pages' downloads were normalized onto the shared saveBlob.
import { assert } from "@std/assert";

const DASHBOARD = new URL("./FilesDashboard.svelte", import.meta.url);
const ROW = new URL("./FileRow.svelte", import.meta.url);
const CHANNEL = new URL("../../pages/ChannelChatPage.svelte", import.meta.url);
const DIRECT = new URL("../../pages/DirectSessionPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "delete is gated behind the danger confirm dialog, no optimistic row drop",
  { permissions: { read: [DASHBOARD] } },
  () => {
    const src = source(DASHBOARD);
    assert(src.includes("<ConfirmDialog"));
    assert(src.includes('tone="danger"'));
    assert(src.includes("open={deleteTarget !== null}"));
    // The list reloads from the server after the mutation.
    assert(src.includes("await load()"));
  },
);

Deno.test(
  "the list states are complete: skeleton, error+retry, both empty shapes",
  { permissions: { read: [DASHBOARD] } },
  () => {
    const src = source(DASHBOARD);
    assert(src.includes('aria-busy="true"'));
    assert(src.includes("animate-pulse"));
    assert(src.includes("files_error()"));
    assert(src.includes("common_retry()"));
    assert(src.includes("files_empty()"));
    assert(src.includes("files_empty_filtered()"));
    // The honest ==500 tail note, not fake pagination.
    assert(src.includes("{#if limitHit}"));
    assert(src.includes("files_limit_note()"));
  },
);

Deno.test(
  "upload picks the area from a dropdown that defaults to shared/",
  { permissions: { read: [DASHBOARD] } },
  () => {
    const src = source(DASHBOARD);
    assert(src.includes('uploadPrefix = $state("shared/")'));
    // The select offers only the server-legal areas (no free-form input)
    // and the 400 stays as the client-side fallback.
    assert(src.includes("<select"));
    assert(src.includes("uploadAreas()"));
    assert(
      !src.includes('type="text" bind:value={uploadPrefix}'),
      "the free-form prefix input must not return",
    );
    assert(src.includes("files_upload_prefix_hint()"));
  },
);

Deno.test(
  "the three page surfaces download through the one saveBlob helper",
  { permissions: { read: [DASHBOARD, ROW, CHANNEL, DIRECT] } },
  () => {
    for (const url of [DASHBOARD, CHANNEL, DIRECT]) {
      const src = source(url);
      assert(src.includes("saveBlob("));
      // The inline triple (createObjectURL + synthetic click + revoke)
      // must not reappear: download I/O rides the shared saveBlob helper.
      assert(
        !src.includes("URL.createObjectURL"),
        "download I/O must ride the shared saveBlob helper",
      );
    }
  },
);

Deno.test(
  "the row wires its actions and surfaces a failed action inline",
  { permissions: { read: [ROW] } },
  () => {
    const src = source(ROW);
    assert(src.includes("files_download_aria({ name: row.displayPath })"));
    assert(src.includes("onDownload(row)"));
    assert(src.includes("onDelete(row)"));
    assert(src.includes("{#if error}"));
    assert(src.includes("{highlighted"));
  },
);
