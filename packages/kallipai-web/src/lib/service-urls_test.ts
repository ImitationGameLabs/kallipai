import { assertEquals } from "@std/assert";
import { serviceUrl } from "./service-urls.ts";

const page = { protocol: "https:", hostname: "app.kallipai.com" };

Deno.test(
  "derives the api origin with per-service bases from an https page",
  () => {
    assertEquals(
      serviceUrl("archeion", {}, page),
      "https://api.kallipai.com/v1/archeion",
    );
    assertEquals(
      serviceUrl("lesche", {}, page),
      "https://api.kallipai.com/v1/lesche",
    );
    assertEquals(
      serviceUrl("files", {}, page),
      "https://api.kallipai.com/v1/files",
    );
    assertEquals(
      serviceUrl("instances", {}, page),
      "https://api.kallipai.com/v1/instances",
    );
  },
);

Deno.test("api face follows the page protocol on plain http", () => {
  const http = { protocol: "http:", hostname: "app.kallipai.lan" };
  assertEquals(
    serviceUrl("archeion", {}, http),
    "http://api.kallipai.lan/v1/archeion",
  );
});

Deno.test("strips the app. prefix from the page hostname", () => {
  assertEquals(
    serviceUrl(
      "lesche",
      {},
      { protocol: "https:", hostname: "app.example.org" },
    ),
    "https://api.example.org/v1/lesche",
  );
});

Deno.test("config.domain overrides the hostname-derived domain", () => {
  assertEquals(
    serviceUrl(
      "files",
      { domain: "kallipai.com" },
      {
        protocol: "http:",
        hostname: "localhost",
      },
    ),
    "http://api.kallipai.com/v1/files",
  );
});

Deno.test("explicit apiBase override wins over derivation", () => {
  assertEquals(
    serviceUrl("archeion", { apiBase: "http://10.0.0.7:8080" }, page),
    "http://10.0.0.7:8080/v1/archeion",
  );
});

Deno.test("apiBase override trims a trailing slash", () => {
  assertEquals(
    serviceUrl("files", { apiBase: "https://edge.example.com/" }, page),
    "https://edge.example.com/v1/files",
  );
});

Deno.test("an empty apiBase counts as unset", () => {
  assertEquals(
    serviceUrl("archeion", { apiBase: "" }, page),
    "https://api.kallipai.com/v1/archeion",
  );
});

Deno.test("api face carries a non-default page port", () => {
  assertEquals(
    serviceUrl(
      "archeion",
      {},
      { protocol: "http:", hostname: "app.localhost", port: "8080" },
    ),
    "http://api.localhost:8080/v1/archeion",
  );
});

Deno.test("protocol-default ports stay off the derived url", () => {
  assertEquals(
    serviceUrl(
      "lesche",
      {},
      { protocol: "https:", hostname: "app.kallipai.com", port: "443" },
    ),
    "https://api.kallipai.com/v1/lesche",
  );
  assertEquals(
    serviceUrl(
      "lesche",
      {},
      { protocol: "http:", hostname: "app.kallipai.com", port: "" },
    ),
    "http://api.kallipai.com/v1/lesche",
  );
});

Deno.test("explicit config.domain keeps an app. prefix verbatim", () => {
  assertEquals(
    serviceUrl(
      "archeion",
      { domain: "app.example.com" },
      { protocol: "https:", hostname: "elsewhere.example.com" },
    ),
    "https://api.app.example.com/v1/archeion",
  );
});
