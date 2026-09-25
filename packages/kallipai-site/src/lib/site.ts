// Single point of truth for the site's absolute origin and shared asset
// paths. Every absolute URL the site emits (canonical links, og:url, the
// sitemap, robots.txt) derives from here. The operator has not fixed the
// production domain yet, so the origin below is a placeholder; replacing it
// moves every generated URL at once.
export const siteUrl = "https://kallipai.com";

// Social-card image referenced by the layout's og:image tag. The asset is
// not designed yet; until it exists the tag points at this placeholder
// path.
export const ogImagePath = "/og.png";
