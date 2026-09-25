/** Hand bytes to the browser as a named download: an object URL plus a
 * synthetic link click, revoked on the next tick. The one download path
 * for file attachments (channel chat, direct session) and the files page. */
export function saveBlob(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 0);
}
