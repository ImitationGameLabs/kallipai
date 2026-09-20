// The copy-button feature is wired at three seams: the action module, the
// mount point in DocArticle, and the per-locale labels in both doc routes.
// These assertions pin the seams so a refactor that drops one of them
// fails a test instead of shipping silently. The action's runtime behavior
// needs a DOM and stays covered by manual/build-time checks; here only the
// wiring is pinned.
import assert from "node:assert/strict";

Deno.test("copy-code action is mounted with per-locale labels", async () => {
  const action = await Deno.readTextFile("src/lib/actions/copy-code.ts");
  assert.match(action, /navigator\.clipboard\.writeText/);
  assert.match(action, /data-code-icon=/);

  const article = await Deno.readTextFile(
    "src/lib/components/DocArticle.svelte",
  );
  assert.match(article, /use:enhanceCodeBlocks=\{\{/);
  // The icons come from @lucide/svelte via DocArticle's hidden template,
  // which the action clones; pin the template and the cloning seam.
  assert.match(article, /data-code-icon="copy"/);
  assert.match(article, /data-code-icon="check"/);
  assert.match(article, /copyCode: string/);

  for (const route of ["en", "zh-cn"]) {
    const page = await Deno.readTextFile(
      `src/routes/${route}/docs/[...slug]/+page.svelte`,
    );
    assert.match(page, /copyCode:/);
    assert.match(page, /copied:/);
    assert.match(page, /failed:/);
  }
});
