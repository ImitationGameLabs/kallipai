// Catalog guard: message catalogs live per domain under
// ../i18n/project.inlang/messages/{locale}/<domain>.json. This suite pins the
// catalog's structural conventions so drift fails here instead of surfacing
// at runtime: locale parity (same keys, same {placeholder} sets), no key in
// two files, the closed prefix vocabulary, banned role abbreviations,
// plural-pair integrity, and the same-value synonym allowlist. The file list
// is pinned against settings.json's pathPattern so a catalog file added to
// the build but not this guard (or vice versa) fails loudly.
//
// The loader table uses string-literal dynamic imports on purpose: literals
// join the test runner's module graph at load time, while interpolated
// imports would hit the runtime read-permission wall.
import { assertEquals } from "@std/assert";

const FILES = [
  "common",
  "shell",
  "auth",
  "connect",
  "chat",
  "signal",
  "user",
  "tagma",
  "tagmata",
  "room",
  "rooms",
  "roomsettings",
  "settings",
  "manage",
  "manage_overview",
  "manage_agent",
  "manage_agents",
  "manage_budget",
  "manage_profiles",
  "manage_schedules",
] as const;

type Loader = () => Promise<Record<string, string>>;

const loaders: Record<string, Loader> = {
  "en|common": () =>
    import("../i18n/project.inlang/messages/en/common.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|shell": () =>
    import("../i18n/project.inlang/messages/en/shell.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|auth": () =>
    import("../i18n/project.inlang/messages/en/auth.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|connect": () =>
    import("../i18n/project.inlang/messages/en/connect.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|chat": () =>
    import("../i18n/project.inlang/messages/en/chat.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|signal": () =>
    import("../i18n/project.inlang/messages/en/signal.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|user": () =>
    import("../i18n/project.inlang/messages/en/user.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|tagma": () =>
    import("../i18n/project.inlang/messages/en/tagma.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|tagmata": () =>
    import("../i18n/project.inlang/messages/en/tagmata.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|room": () =>
    import("../i18n/project.inlang/messages/en/room.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|rooms": () =>
    import("../i18n/project.inlang/messages/en/rooms.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|roomsettings": () =>
    import("../i18n/project.inlang/messages/en/roomsettings.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|settings": () =>
    import("../i18n/project.inlang/messages/en/settings.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage": () =>
    import("../i18n/project.inlang/messages/en/manage.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_overview": () =>
    import("../i18n/project.inlang/messages/en/manage_overview.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_agent": () =>
    import("../i18n/project.inlang/messages/en/manage_agent.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_agents": () =>
    import("../i18n/project.inlang/messages/en/manage_agents.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_budget": () =>
    import("../i18n/project.inlang/messages/en/manage_budget.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_profiles": () =>
    import("../i18n/project.inlang/messages/en/manage_profiles.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_schedules": () =>
    import("../i18n/project.inlang/messages/en/manage_schedules.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|common": () =>
    import("../i18n/project.inlang/messages/zh/common.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|shell": () =>
    import("../i18n/project.inlang/messages/zh/shell.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|auth": () =>
    import("../i18n/project.inlang/messages/zh/auth.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|connect": () =>
    import("../i18n/project.inlang/messages/zh/connect.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|chat": () =>
    import("../i18n/project.inlang/messages/zh/chat.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|signal": () =>
    import("../i18n/project.inlang/messages/zh/signal.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|user": () =>
    import("../i18n/project.inlang/messages/zh/user.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|tagma": () =>
    import("../i18n/project.inlang/messages/zh/tagma.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|tagmata": () =>
    import("../i18n/project.inlang/messages/zh/tagmata.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|room": () =>
    import("../i18n/project.inlang/messages/zh/room.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|rooms": () =>
    import("../i18n/project.inlang/messages/zh/rooms.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|roomsettings": () =>
    import("../i18n/project.inlang/messages/zh/roomsettings.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|settings": () =>
    import("../i18n/project.inlang/messages/zh/settings.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage": () =>
    import("../i18n/project.inlang/messages/zh/manage.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_overview": () =>
    import("../i18n/project.inlang/messages/zh/manage_overview.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_agent": () =>
    import("../i18n/project.inlang/messages/zh/manage_agent.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_agents": () =>
    import("../i18n/project.inlang/messages/zh/manage_agents.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_budget": () =>
    import("../i18n/project.inlang/messages/zh/manage_budget.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_profiles": () =>
    import("../i18n/project.inlang/messages/zh/manage_profiles.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_schedules": () =>
    import("../i18n/project.inlang/messages/zh/manage_schedules.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
};

async function loadCatalog(locale: string): Promise<Record<string, string>> {
  const merged: Record<string, string> = {};
  for (const name of FILES) {
    const mod = await loaders[`${locale}|${name}`]();
    for (const [key, value] of Object.entries(mod)) {
      if (key === "$schema") continue;
      if (key in merged) {
        throw new Error(`duplicate key ${key} (also in ${name}.json)`);
      }
      merged[key] = value;
    }
  }
  return merged;
}

const en = await loadCatalog("en");
const zh = await loadCatalog("zh");

function placeholders(value: string): string[] {
  return [...value.matchAll(/\{([a-zA-Z][a-zA-Z0-9_]*)\}/g)]
    .map((m) => m[1]!)
    .sort();
}

Deno.test(
  "catalog: settings.json pathPattern matches the guard file list",
  async () => {
    const settings = (
      await import("../i18n/project.inlang/settings.json", {
        with: { type: "json" },
      })
    ).default as { "plugin.inlang.messageFormat": { pathPattern: string[] } };
    const fromSettings = settings["plugin.inlang.messageFormat"].pathPattern
      .map((p) => {
        const m = p.match(/\/([a-z_]+)\.json$/);
        if (!m) {
          throw new Error(`pathPattern entry is not a catalog file: ${p}`);
        }
        return m[1]!;
      })
      .sort();
    assertEquals(fromSettings, [...FILES].sort());
  },
);

Deno.test("catalog: en and zh share keys and placeholders", () => {
  const enKeys = Object.keys(en).sort();
  const zhKeys = Object.keys(zh).sort();
  assertEquals(enKeys, zhKeys);
  for (const key of enKeys) {
    assertEquals(
      placeholders(en[key] ?? ""),
      placeholders(zh[key] ?? ""),
      `placeholder mismatch for ${key}`,
    );
  }
});

Deno.test("catalog: values are non-empty in both locales", () => {
  for (const [key, value] of Object.entries(en)) {
    if (value.trim() === "") throw new Error(`${key}: empty en value`);
    if ((zh[key] ?? "").trim() === "") {
      throw new Error(`${key}: empty zh value`);
    }
  }
});

// Prefix vocabulary is closed: a key's prefix names its domain (cross-surface
// copy like auth_/signal_) or its page (nav_/settings_). The bare manage_
// prefix is reserved for manage-section layout state shared across pages.
const PREFIXES = new Set([
  "common",
  "error",
  "remaining",
  "agent_state",
  "shell",
  "shell_status",
  "connection",
  "nav",
  "account",
  "auth",
  "login",
  "oauth",
  "register",
  "pair",
  "connect",
  "chat",
  "composer",
  "sender",
  "timeline",
  "signal",
  "user",
  "tagma",
  "tagmata",
  "room",
  "rooms",
  "roomsettings",
  "settings",
  "manage",
  "manage_overview",
  "manage_agent",
  "manage_agents",
  "manage_budget",
  "manage_profiles",
  "manage_schedules",
]);
const BARE_MANAGE_KEYS = new Set(["manage_opening"]);

Deno.test("catalog: key prefixes stay inside the closed vocabulary", () => {
  for (const key of Object.keys(en)) {
    const two = key.split("_").slice(0, 2).join("_");
    const prefix = PREFIXES.has(two) ? two : key.split("_")[0]!;
    if (!PREFIXES.has(prefix)) {
      throw new Error(
        `unknown prefix "${prefix}" on ${key}: add it to PREFIXES (and the catalog docs) or rename the key`,
      );
    }
    if (prefix === "manage" && !BARE_MANAGE_KEYS.has(key)) {
      throw new Error(
        `${key}: bare manage_ is reserved for section-shared layout keys; use a manage_<page>_ prefix`,
      );
    }
    if (key.includes("_err_")) {
      throw new Error(`${key}: use the _error suffix, not _err_`);
    }
  }
});

// Prefix-to-file attribution: several prefixes deliberately live inside a
// differently-named file (agent_state_ in common.json, connection_ and nav_
// in shell.json). This map makes the attribution explicit and fails when a
// key lands in a file its prefix is not attributed to, so "split by domain"
// stays enforced rather than aspirational.
const PREFIX_FILES: Record<string, string> = {
  common: "common",
  error: "common",
  remaining: "common",
  agent_state: "common",
  shell: "shell",
  shell_status: "shell",
  connection: "shell",
  nav: "shell",
  account: "shell",
  auth: "auth",
  login: "auth",
  oauth: "auth",
  register: "auth",
  pair: "auth",
  connect: "connect",
  chat: "chat",
  composer: "chat",
  sender: "chat",
  timeline: "chat",
  signal: "signal",
  user: "user",
  tagma: "tagma",
  tagmata: "tagmata",
  room: "room",
  rooms: "rooms",
  roomsettings: "roomsettings",
  settings: "settings",
  manage: "manage",
  manage_overview: "manage_overview",
  manage_agent: "manage_agent",
  manage_agents: "manage_agents",
  manage_budget: "manage_budget",
  manage_profiles: "manage_profiles",
  manage_schedules: "manage_schedules",
};

Deno.test(
  "catalog: each key lives in its prefix's attributed file",
  async () => {
    for (const name of FILES) {
      const mod = await loaders[`en|${name}`]();
      for (const key of Object.keys(mod)) {
        if (key === "$schema") continue;
        const two = key.split("_").slice(0, 2).join("_");
        const prefix = PREFIXES.has(two) ? two : key.split("_")[0]!;
        const expected = PREFIX_FILES[prefix];
        if (expected !== name) {
          throw new Error(
            `${key} (${prefix}) is in ${name}.json but attributed to ${expected}.json`,
          );
        }
      }
    }
  },
);

// Role-word hygiene: a few ambiguous abbreviations are banned anywhere in a
// key (not just as the final token), because they read as typos of the full
// word (error/button/text/message) in every position.
const BANNED_ROLE_WORDS = ["err", "btn", "txt", "msg", "caption"];

Deno.test("catalog: keys avoid banned role abbreviations", () => {
  for (const key of Object.keys(en)) {
    for (const word of BANNED_ROLE_WORDS) {
      if (key.includes(`_${word}_`) || key.endsWith(`_${word}`)) {
        throw new Error(
          `${key}: banned role word "${word}"; use the full word (error/button/message)`,
        );
      }
    }
  }
});

Deno.test("catalog: _one/_other appear only as complete plural pairs", () => {
  const keys = Object.keys(en);
  for (const key of keys) {
    if (key.endsWith("_one")) {
      if (!keys.includes(`${key.slice(0, -4)}_other`)) {
        throw new Error(`${key}: _one without a matching _other`);
      }
    } else if (key.endsWith("_other")) {
      if (!keys.includes(key.replace(/_other$/, "_one"))) {
        throw new Error(`${key}: _other without a matching _one`);
      }
    }
  }
});

// Same-value keys are allowed only as deliberate synonyms (nav labels that
// may drift from page headings on purpose). Checked against the EN catalog
// only: zh translations collide far more often (34 groups today) and those
// collisions are accepted as translation coincidence, not guarded.
const SYNONYMS: string[][] = [
  ["settings_heading", "rooms_menu_settings"],
  ["connection_connecting", "shell_connecting"],
  ["nav_manage", "tagma_menu_manage"],
  ["nav_overview", "manage_overview_heading"],
  ["nav_budget", "manage_budget_heading"],
  ["nav_agents", "manage_agents_heading"],
  ["nav_profiles", "manage_profiles_heading"],
  ["nav_schedules", "manage_schedules_heading"],
  ["tagma_presence_online", "room_member_online_aria"],
  ["tagma_presence_offline", "room_member_offline_aria", "shell_offline"],
  ["auth_creating", "rooms_creating"],
  ["login_username", "auth_username"],
  ["rooms_name_label", "manage_schedules_name"],
  ["room_settings_aria", "roomsettings_subtitle"],
  ["roomsettings_removing", "tagma_rooms_removing"],
  ["roomsettings_remove_failed", "tagma_rooms_remove_failed"],
  ["composer_message_aria", "tagma_profile_message"],
  ["manage_profiles_model_placeholder", "manage_profiles_profile_model_label"],
  [
    "manage_profiles_max_context_placeholder",
    "manage_profiles_max_context_label",
  ],
];

Deno.test("catalog: same-value keys are all deliberate synonyms", () => {
  const byValue = new Map<string, string[]>();
  for (const [key, value] of Object.entries(en)) {
    const group = byValue.get(value) ?? [];
    group.push(key);
    byValue.set(value, group);
  }
  const allowed = new Set(SYNONYMS.map((g) => [...g].sort().join(",")));
  for (const [value, group] of byValue) {
    if (group.length < 2) continue;
    const sig = [...group].sort().join(",");
    if (!allowed.has(sig)) {
      throw new Error(
        `unlisted same-value group [${sig}] for "${value}": reuse a key or add the pair to SYNONYMS`,
      );
    }
  }
});
