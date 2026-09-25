// Physical-locale routing: every page lives under its locale prefix
// (/en/**, /zh-cn/**). The bare root is a language-detection shell that
// lands on /en/, so the two trees stay symmetric. The header holds the
// site's only language switch, so the /en/ <-> /zh-cn/ jump stays
// consistent there; the footer just detects the locale to render.

export function isLocaleZh(pathname: string): boolean {
  // Strict segment match: "/zh-cn-abc" is not the zh-cn locale.
  return pathname === "/zh-cn" || pathname.startsWith("/zh-cn/");
}
export function isLocaleEn(pathname: string): boolean {
  return pathname === "/en" || pathname.startsWith("/en/");
}
// Same-path href in the other locale; "/" resolves to each tree's home.
export function mirrorHref(pathname: string): string {
  if (isLocaleZh(pathname)) {
    const rest = pathname.slice("/zh-cn".length);
    return rest === "" ? "/en/" : `/en${rest}`;
  }
  if (isLocaleEn(pathname)) {
    const rest = pathname.slice("/en".length);
    return rest === "" ? "/zh-cn/" : `/zh-cn${rest}`;
  }
  return "/en/";
}

// The en-tree href for a bare canonical path.
export function enHref(pathname: string): string {
  return pathname === "/" ? "/en/" : `/en${pathname}`;
}

// The zh-tree href for a bare canonical path.
export function zhHref(pathname: string): string {
  return pathname === "/" ? "/zh-cn/" : `/zh-cn${pathname}`;
}
