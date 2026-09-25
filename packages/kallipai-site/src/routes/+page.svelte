<svelte:head>
  <!-- Language-detection shell: the bare root hosts no content. The inline
      script runs before the meta refresh fires, so script-capable browsers
      land in their language and the refresh stays as the no-JS fallback. -->
  <title>KallipAI</title>
  <meta http-equiv="refresh" content="0; url=/en/" />
  {@html '<script>if ((navigator.languages || [navigator.language]).some((l) => l && l.toLowerCase().startsWith("zh"))) location.replace("/zh-cn/");</script>'}
</svelte:head>
