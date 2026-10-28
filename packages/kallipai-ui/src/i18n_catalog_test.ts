// Catalog guard: message catalogs live per domain under
// ../i18n/project.inlang/messages/{locale}/<domain>.json. This suite pins the
// catalog's structural conventions so drift fails here instead of surfacing
// at runtime: locale parity (same keys, same {placeholder} sets), no key in
// two files, the closed prefix vocabulary, banned role abbreviations,
// plural-pair integrity, and the same-value synonym allowlist. The file list
// is pinned against settings.json's pathPattern so a catalog file added to
// the build but not this guard (or vice versa) fails loudly.
// A final sweep holds every catalog key to at least one mention in source.
//
// The loader table uses string-literal dynamic imports on purpose: literals
// join the test runner's module graph at load time, while interpolated
// imports would hit the runtime read-permission wall.
import { assert, assertEquals, assertFalse } from "@std/assert";

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
  "manage_gateway",
  "manage_schedules",
  "manage_tasks",
  "manage_turns",
  "manage_instances",
  "user_gateway",
  "panorama",
  "files",
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
  "en|manage_gateway": () =>
    import("../i18n/project.inlang/messages/en/manage_gateway.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_schedules": () =>
    import("../i18n/project.inlang/messages/en/manage_schedules.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_tasks": () =>
    import("../i18n/project.inlang/messages/en/manage_tasks.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_instances": () =>
    import("../i18n/project.inlang/messages/en/manage_instances.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|panorama": () =>
    import("../i18n/project.inlang/messages/en/panorama.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|files": () =>
    import("../i18n/project.inlang/messages/en/files.json", {
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
  "en|user_gateway": () =>
    import("../i18n/project.inlang/messages/en/user_gateway.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_gateway": () =>
    import("../i18n/project.inlang/messages/zh/manage_gateway.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_schedules": () =>
    import("../i18n/project.inlang/messages/zh/manage_schedules.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_tasks": () =>
    import("../i18n/project.inlang/messages/zh/manage_tasks.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "en|manage_turns": () =>
    import("../i18n/project.inlang/messages/en/manage_turns.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_turns": () =>
    import("../i18n/project.inlang/messages/zh/manage_turns.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|manage_instances": () =>
    import("../i18n/project.inlang/messages/zh/manage_instances.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|user_gateway": () =>
    import("../i18n/project.inlang/messages/zh/user_gateway.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|panorama": () =>
    import("../i18n/project.inlang/messages/zh/panorama.json", {
      with: { type: "json" },
    }).then((m) => m.default as Record<string, string>),
  "zh|files": () =>
    import("../i18n/project.inlang/messages/zh/files.json", {
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
  "admin",
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
  "manage_instances",
  "manage_tasks",
  "manage_turns",
  "manage_gateway",
  "user_gateway",
  "panorama",
  "files",
]);
const BARE_MANAGE_KEYS = new Set(["manage_opening", "manage_backend_failed"]);

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
// differently-named file (agent_state_ in common.json, connection_, nav_,
// and admin_ in shell.json). This map makes the attribution explicit and
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
  admin: "shell",
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
  manage_instances: "manage_instances",
  manage_tasks: "manage_tasks",
  manage_turns: "manage_turns",
  manage_gateway: "manage_gateway",
  user_gateway: "user_gateway",
  panorama: "panorama",
  files: "files",
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

// Same-value keys are allowed only as deliberate synonyms — nav labels
// that may drift from page headings on purpose, and field labels shared
// across dialogs. Checked against the EN catalog only: zh translations
// collide far more often and those collisions are accepted as
// translation coincidence, not guarded.
const SYNONYMS: string[][] = [
  ["files_heading", "nav_files"],
  [
    "manage_gateway_provider_actions_aria",
    "manage_profiles_provider_actions_aria",
  ],
  ["manage_gateway_draft_badge", "user_gateway_profile_draft"],
  [
    "manage_gateway_placement_collection_label",
    "user_gateway_profile_collection",
    "user_gateway_set_dialog_collection_label",
  ],
  ["manage_gateway_set_add", "user_gateway_set_add"],
  ["manage_profiles_tab_local", "manage_schedules_clock_local"],
  ["manage_gateway_provider_key", "user_gateway_provider_key"],
  [
    "manage_gateway_provider_family",
    "manage_profiles_provider_family_label",
    "user_gateway_provider_family",
  ],
  [
    "manage_gateway_provider_pool_empty",
    "user_gateway_profile_provider_pool_empty",
  ],
  [
    "manage_profiles_profile_dialog_effort_unset",
    "user_gateway_profile_effort_unset",
  ],
  [
    "manage_profiles_profile_modalities_label",
    "manage_profiles_set_modalities_label",
  ],
  ["files_download_aria", "chat_file_download_aria"],
  ["chat_attachment_remove", "chat_mention_remove_aria"],
  ["account_menu", "settings_account"],
  ["settings_heading", "rooms_menu_settings"],
  ["connection_connecting", "shell_connecting"],
  ["nav_manage", "tagma_menu_manage"],
  ["tagma_rooms_section_manage", "tagma_menu_manage_rooms"],
  ["nav_overview", "manage_overview_heading"],
  ["nav_budget", "manage_budget_heading"],
  ["nav_agents", "manage_agents_heading"],
  ["nav_profiles", "manage_profiles_heading", "user_gateway_profiles"],
  ["nav_schedules", "manage_schedules_heading"],
  ["nav_tasks", "manage_tasks_heading"],
  ["manage_instances_token_apply", "manage_profiles_apply"],
  ["tagma_presence_online", "room_member_online_aria"],
  ["tagma_presence_offline", "room_member_offline_aria", "shell_offline"],
  ["manage_tasks_col_archived", "manage_tasks_show_archived"],
  ["auth_creating", "rooms_creating"],
  ["login_username", "auth_username"],
  ["auth_couldnt_reach", "settings_email_failed"],
  ["room_settings_aria", "roomsettings_subtitle"],
  ["roomsettings_removing", "tagma_rooms_removing"],
  ["roomsettings_remove_failed", "tagma_rooms_remove_failed"],
  ["composer_message_aria", "tagma_profile_message"],
  ["manage_profiles_model_placeholder", "manage_profiles_profile_model_label"],
  [
    "manage_profiles_max_context_placeholder",
    "manage_profiles_max_context_label",
  ],
  [
    "user_gateway_profile_provider",
    "manage_agent_provider",
    "manage_profiles_set_dialog_provider_label",
    "manage_profiles_profile_dialog_endpoint_label",
    "manage_gateway_provider_label",
  ],
  ["manage_agent_retry_error_unknown", "shell_error"],
  [
    "user_gateway_profile_model",
    "manage_agent_model",
    "manage_profiles_set_dialog_model_label",
    "manage_profiles_profile_dialog_model_label",
  ],
  [
    "manage_profiles_profile_dialog_id_label",
    "manage_profiles_set_dialog_id_label",
  ],
  [
    "manage_profiles_profile_dialog_max_context_label",
    "manage_profiles_set_dialog_max_context_label",
  ],
  ["manage_profiles_set_dialog_description_label", "rooms_description_label"],
  ["manage_agent_set", "manage_gateway_placement_set_label"],
  ["manage_profiles_set_dialog_edit_title", "user_gateway_set_edit_title"],
  ["manage_instances_dialog_title", "manage_instances_new"],
  ["rooms_name_label", "user_gateway_set_name"],
  ["tagma_drawer_title", "nav_breadcrumb_agents"],
  ["manage_budget_unlimited", "tagma_status_unlimited"],
  ["manage_gateway_provider_edit_title", "user_gateway_provider_edit_title"],
  ["manage_gateway_provider_add", "user_gateway_provider_add"],
  ["manage_gateway_provider_id", "user_gateway_provider_id"],
  ["manage_gateway_sets", "manage_profiles_sets", "user_gateway_sets"],
  ["user_gateway_publish", "manage_gateway_collection_publish"],
  ["user_gateway_unpublish", "manage_gateway_collection_unpublish"],
  ["manage_gateway_collections", "user_gateway_collections"],
  [
    "manage_gateway_collection_add",
    "manage_gateway_collection_dialog_create_title",
    "user_gateway_collection_add",
  ],
  [
    "manage_gateway_collection_dialog_edit_title",
    "user_gateway_collection_edit_title",
  ],
  ["manage_gateway_reach_everyone", "user_gateway_everyone"],
  [
    "manage_gateway_group_add",
    "manage_gateway_group_dialog_create_title",
    "user_gateway_group_add",
  ],
  ["manage_gateway_group_dialog_edit_title", "user_gateway_group_edit_title"],
  ["manage_gateway_group_members", "room_members"],
  ["manage_gateway_group_members_count_one", "room_member_one"],
  ["manage_gateway_group_members_count_other", "room_member_other"],
  [
    "manage_gateway_group_search_placeholder",
    "manage_gateway_group_search_aria",
  ],
  [
    "manage_gateway_group_name_label",
    "rooms_name_label",
    "user_gateway_set_name",
  ],
  ["manage_gateway_groups", "user_gateway_groups"],
  ["manage_gateway_provider_base", "user_gateway_credential_base"],
  ["user_gateway_heading", "user_gateway_menu"],
  ["manage_agent_description", "user_gateway_set_description"],
  ["manage_profiles_providers", "user_gateway_providers"],
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

// A catalog key nothing references is dead weight: it survives refactors,
// keeps a stale translation alive in both locales, and no other test notices.
// This sweep collects the identifiers mentioned across the package sources and
// holds every catalog key to at least one of them. It reads source text rather
// than importing modules, so the check stays structural -- no render, no
// locale setup.
//
// Only `<!-- ... -->` spans are blanked out, in any file: markup's one comment
// form, and the one that would otherwise let a commented-out key vouch for
// itself. Separating JS comments from code would take a lexer, whose failure
// modes -- an apostrophe in body prose, a quote inside a regex literal, a
// comment inside a `{...}` expression, a closing tag inside a string -- cost
// more than the mentions it would catch. A key mentioned only in a JS comment
// therefore counts as referenced and stays.
//
// This test's own file is skipped because SYNONYMS lists keys as data, so a
// key parked there would otherwise vouch for itself, and the generated
// paraglide modules are skipped because they hold every key by construction.
//
// SOURCE_ROOTS names the packages that consume this catalog: a new consumer
// has to be added here, or its keys read as unreferenced. There is no
// exemption list -- a key reported here is either used or deleted; if an
// exemption is ever needed, say why in the same change.
const PACKAGE_ROOT = new URL("../", import.meta.url);
const GUARD_FILE = new URL("./i18n_catalog_test.ts", import.meta.url);
const SOURCE_ROOTS = [
  "./src",
  "../kallipai-app/src",
  "../kallipai-direct/src",
  "../kallipai-web/src",
].map((p) => new URL(`${p}/`, PACKAGE_ROOT));
const SOURCE_EXTS = [".ts", ".svelte", ".js"];
const SKIP_DIRS = ["build", "coverage", "dist", "node_modules", "paraglide"];
const IDENT = /[A-Za-z_$][A-Za-z0-9_$]*/g;

/**
 * Blank out every `<!-- ... -->` span, in any file -- markup's one comment
 * form, and no file gets a rule of its own. Every other byte stays as written,
 * a script string included. An unclosed span runs to the end of the file, so a
 * key after it reads as unreferenced.
 */
function stripMarkupComments(text: string): string {
  let out = "";
  let i = 0;
  while (i < text.length) {
    if (text.startsWith("<!--", i)) {
      const end = text.indexOf("-->", i + 4);
      i = end === -1 ? text.length : end + 3;
      out += " ";
      continue;
    }
    out += text.charAt(i);
    i += 1;
  }
  return out;
}

/** Collect the identifiers one source file mentions. */
function mentionsIn(text: string, into: Set<string>): void {
  for (const m of stripMarkupComments(text).matchAll(IDENT)) into.add(m[0]!);
}

async function collectMentions(dir: URL, into: Set<string>): Promise<void> {
  for await (const entry of Deno.readDir(dir)) {
    if (
      entry.isSymlink ||
      entry.name.startsWith(".") ||
      SKIP_DIRS.includes(entry.name)
    ) {
      continue;
    }
    const child = new URL(`${entry.name}${entry.isDirectory ? "/" : ""}`, dir);
    if (entry.isDirectory) {
      await collectMentions(child, into);
      continue;
    }
    if (child.href === GUARD_FILE.href) continue;
    if (!SOURCE_EXTS.some((ext) => entry.name.endsWith(ext))) continue;
    const text = new TextDecoder().decode(Deno.readFileSync(child));
    mentionsIn(text, into);
  }
}

Deno.test(
  "catalog: every key is mentioned by package source",
  { permissions: { read: SOURCE_ROOTS } },
  async () => {
    const mentioned = new Set<string>();
    for (const root of SOURCE_ROOTS) await collectMentions(root, mentioned);
    const unreferenced = Object.keys(en)
      .filter((key) => !mentioned.has(key))
      .sort();
    assertEquals(
      unreferenced,
      [],
      "unreferenced catalog keys: use them, or delete them from both locales",
    );
  },
);

Deno.test("catalog: a markup comment cannot vouch for a key", () => {
  // Prose apostrophes, an apostrophe inside a script string and an unquoted
  // url attribute all sit in one file: none of them needs special reading, and
  // the markup comment still cannot vouch for a key.
  const svelte = [
    "<script>",
    '  const note = "Don\'t";',
    "  const liveMention = 1;",
    "</script>",
    "<p>Don't take what you can't use</p>",
    "<a href=https://example.test/x>linkMention</a>",
    "<p>manage_<!-- dead -->gateway</p>",
    "<!-- deadMarkupKey -->",
    "<p>{liveMention}</p>",
  ].join("\n");
  const ids = new Set<string>();
  mentionsIn(svelte, ids);
  assert(ids.has("liveMention"));
  assert(ids.has("linkMention"));
  assertFalse(ids.has("deadMarkupKey"));
  assertFalse(ids.has("manage_gateway"));
});

Deno.test(
  "catalog: a name in a markup expression or a script string counts",
  () => {
    const svelte = [
      "<script>",
      '  const fromString = "stringMention";',
      "</script>",
      "<p>{exprMention}</p>",
    ].join("\n");
    const ids = new Set<string>();
    mentionsIn(svelte, ids);
    assert(ids.has("stringMention"));
    assert(ids.has("exprMention"));
  },
);

Deno.test("catalog: an unclosed markup comment hides the text after it", () => {
  const svelte = [
    "<script>",
    "  const liveMention = 1;",
    "</script>",
    "<p>{liveMention}</p>",
    "<!-- unclosed",
    "<p>{afterUnclosed}</p>",
  ].join("\n");
  const ids = new Set<string>();
  mentionsIn(svelte, ids);
  assert(ids.has("liveMention"));
  assertFalse(ids.has("afterUnclosed"));
});

Deno.test(
  "catalog: a comment is blanked without a script body around it",
  () => {
    const text = "<p>{beforeMention}</p><!-- deadKey --><p>{afterMention}</p>";
    const ids = new Set<string>();
    mentionsIn(text, ids);
    assert(ids.has("beforeMention"));
    assert(ids.has("afterMention"));
    assertFalse(ids.has("deadKey"));
  },
);

Deno.test("catalog: a name in a js comment counts too", () => {
  // The named price of blanking only `<!-- ... -->`: a key mentioned only in
  // a comment keeps its catalog entry.
  const ts = [
    "// lineCommentMention",
    "/* blockCommentMention */",
    "const used = 1; // trailingCommentMention",
  ].join("\n");
  const ids = new Set<string>();
  mentionsIn(ts, ids);
  assert(ids.has("lineCommentMention"));
  assert(ids.has("blockCommentMention"));
  assert(ids.has("trailingCommentMention"));
  assert(ids.has("used"));
});
