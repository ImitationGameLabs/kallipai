// Ambient route-data types. A page load may provide these SEO fields; the
// root layout promotes them to og:title / og:description when present, so
// dynamic pages (docs) drive their card tags from one data source.
declare global {
  namespace App {
    interface PageData {
      seoTitle?: string;
      seoDescription?: string;
    }
  }
}

export {};
