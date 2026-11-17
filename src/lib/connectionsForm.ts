// Form logic of the connection editor (CYBERNEURA-DEV-1299): which fields each engine uses,
// turning the loaded `servers` into editable items and back into the save request. Pure functions
// so tests/connections-form.test.mts can call them. The backend (src-tauri/src/connections.rs)
// fills in the secrets, checks the whole list with the app's parser, and writes it.

/// One secret change (settings.rs `Change`)
export type SecretChange = { op: "set"; value: string } | { op: "unset" };

export const SECRET_KEYS = ["password", "ssh_tunnel.password", "ssh_tunnel.private_key_passphrase"] as const;
export type SecretKey = (typeof SECRET_KEYS)[number];

export interface ConnectionsEditorView {
  config_path: string;
  editable: boolean;
  readonly_reason: string | null;
  servers_overridden: boolean;
  override_error: string | null;
  has_comments: boolean;
  backup_path: string;
  backup_exists: boolean;
  local: { servers: unknown[]; secrets: Record<string, string[]>; revision: string } | null;
  templates: { name: string; engine: string | null }[];
  engines: string[];
}

/// A server entry being edited. `value` is the entry without secrets; keys the form does not
/// know stay in it and are written back.
export interface ServerItem {
  kind: "server";
  /// Stable id for the list (not saved)
  id: number;
  /// Index path in the file this entry was read from (null = new / duplicated)
  origin: string | null;
  /// A duplicate copies the secrets of this entry
  copySecretsFrom: string | null;
  value: Record<string, unknown>;
  /// Secret keys set in the file for the source entry
  secretsSet: string[];
  secretEdits: Partial<Record<SecretKey, SecretChange>>;
  /// The effective engine when it was loaded (null for new entries). Fields the engine does not
  /// use are dropped on save only when the engine changed: an untouched entry is written back as
  /// it was (e.g. a sqlite path kept in `host`, `tls: true` on postgres)
  originalEngine: string | null;
}

/// Template name → engine, for entries that inherit `engine` from a template
export type TemplateEngines = Record<string, string | null>;

export const templateEngines = (templates: ConnectionsEditorView["templates"]): TemplateEngines =>
  Object.fromEntries(templates.map((t) => [t.name, t.engine]));

/// The engine an entry ends up with: its own, or its template's
export function effectiveEngine(value: Record<string, unknown>, templates: TemplateEngines): string {
  if (typeof value.engine === "string" && value.engine !== "") return canonicalEngine(value.engine);
  const fromTemplate = typeof value.template === "string" ? templates[value.template] : null;
  return fromTemplate ? canonicalEngine(fromTemplate) : "";
}

export interface GroupItem {
  kind: "group";
  id: number;
  groupName: string;
  color: string;
  servers: ServerItem[];
}

export type Item = ServerItem | GroupItem;

/// Fields shown for each engine (besides the common ones: name, description, engine, template,
/// readonly, allow_dangerous_statements, color, folder_name)
export const ENGINE_FIELDS: Record<string, string[]> = {
  postgres: ["host", "port", "schema", "user", "password", "ssl_mode", "ssl_root_cert", "ssh_tunnel"],
  mysql: ["host", "port", "schema", "user", "password", "ssl_mode", "ssl_root_cert", "ssh_tunnel"],
  mssql: ["host", "port", "schema", "user", "password", "ssl_mode", "ssl_root_cert", "ssh_tunnel"],
  sqlite: ["schema"],
  duckdb: ["schema"],
  redis: ["host", "port", "schema", "user", "password", "ssl_mode", "ssh_tunnel"],
  elasticsearch: ["host", "port", "user", "password", "tls", "ssh_tunnel"],
  dynamodb: ["schema", "host", "port", "tls", "user", "password", "aws_profile"],
};

/// Every key the form knows (and so may drop when the engine does not use it)
const KNOWN_ENGINE_KEYS = [
  "host",
  "port",
  "schema",
  "user",
  "password",
  "password_command",
  "ssl_mode",
  "ssl_root_cert",
  "tls",
  "ssh_tunnel",
  "aws_profile",
];

export const ENGINE_ALIASES: Record<string, string> = {
  postgresql: "postgres",
  mariadb: "mysql",
  sqlserver: "mssql",
  sqlite3: "sqlite",
  valkey: "redis",
  es: "elasticsearch",
  opensearch: "elasticsearch",
};

export const canonicalEngine = (engine: string) => ENGINE_ALIASES[engine.toLowerCase()] ?? engine.toLowerCase();

export function fieldsFor(engine: string): string[] {
  return ENGINE_FIELDS[canonicalEngine(engine)] ?? ENGINE_FIELDS.postgres;
}

/// Label of `schema`, which means something different per engine
export function schemaLabel(engine: string): string {
  switch (canonicalEngine(engine)) {
    case "sqlite":
    case "duckdb":
      return "Database file";
    case "redis":
      return "Database number";
    case "dynamodb":
      return "AWS region";
    default:
      return "Database";
  }
}

export const SSL_MODES = ["", "disable", "prefer", "require", "verify-ca", "verify-full"];

export const PASSWORD_COMMAND_EXAMPLE = 'op read "op://<vault>/<item>/password"';

/// Same warning as docs/settings.md ("Fetching a password at connect time")
export const PASSWORD_COMMAND_WARNING =
  "password_command runs a command on this Mac when the connection is opened. Anyone who can change where it comes from (config.yml, or the YAML that config_override_command returns) can run commands with your permissions.";

const obj = (v: unknown): Record<string, unknown> =>
  v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};

let nextId = 1;
const newId = () => nextId++;

function serverItem(
  value: unknown,
  origin: string,
  secrets: Record<string, string[]>,
  templates: TemplateEngines,
): ServerItem {
  const v = structuredClone(obj(value));
  return {
    kind: "server",
    id: newId(),
    origin,
    copySecretsFrom: null,
    value: v,
    secretsSet: secrets[origin] ?? [],
    secretEdits: {},
    originalEngine: effectiveEngine(v, templates),
  };
}

/// The loaded `servers` as items (groups keep their children)
export function itemsFromView(
  local: NonNullable<ConnectionsEditorView["local"]>,
  templates: TemplateEngines = {},
): Item[] {
  return local.servers.map((entry, i): Item => {
    const e = obj(entry);
    if (Array.isArray(e.servers)) {
      return {
        kind: "group",
        id: newId(),
        groupName: typeof e.group_name === "string" ? e.group_name : "",
        color: typeof e.color === "string" ? e.color : "",
        servers: e.servers.map((child, j) => serverItem(child, `${i}.${j}`, local.secrets, templates)),
      };
    }
    return serverItem(entry, String(i), local.secrets, templates);
  });
}

export function newServer(engine = "postgres"): ServerItem {
  return {
    kind: "server",
    id: newId(),
    origin: null,
    copySecretsFrom: null,
    value: { name: "", engine },
    secretsSet: [],
    secretEdits: {},
    originalEngine: null,
  };
}

/// A copy with a new name. Secrets are copied from the source entry by the backend
/// (secrets the source has changed in the form but not saved are copied as edits).
export function duplicateServer(item: ServerItem, existingNames: string[]): ServerItem {
  const base = typeof item.value.name === "string" && item.value.name ? item.value.name : "connection";
  let name = `${base} copy`;
  for (let n = 2; existingNames.includes(name); n++) name = `${base} copy ${n}`;
  return {
    kind: "server",
    id: newId(),
    origin: null,
    copySecretsFrom: item.origin ?? item.copySecretsFrom,
    value: { ...structuredClone(item.value), name, folder_name: undefined },
    secretsSet: [...item.secretsSet],
    secretEdits: structuredClone(item.secretEdits),
    originalEngine: item.originalEngine,
  };
}

export function allServers(items: Item[]): ServerItem[] {
  return items.flatMap((item) => (item.kind === "group" ? item.servers : [item]));
}

/// Whether a secret will be set after saving (for the form's "stored" hints)
export function secretWillBeSet(item: ServerItem, key: SecretKey): boolean {
  const edit = item.secretEdits[key];
  if (edit) return edit.op === "set";
  return item.secretsSet.includes(key);
}

const engineChanged = (item: ServerItem, templates: TemplateEngines) =>
  item.originalEngine === null || effectiveEngine(item.value, templates) !== item.originalEngine;

/// The value to send. When the engine changed (or for a new entry), keys the new engine does not
/// use are dropped; unknown keys are always kept, and an entry whose engine did not change is
/// written back with every key it had.
export function valueForSave(item: ServerItem, templates: TemplateEngines = {}): Record<string, unknown> {
  const fields = fieldsFor(effectiveEngine(item.value, templates));
  const prune = engineChanged(item, templates);
  const out: Record<string, unknown> = {};
  for (const [key, v] of Object.entries(item.value)) {
    if (v === undefined) continue;
    if (prune && KNOWN_ENGINE_KEYS.includes(key)) {
      const used = key === "password_command" ? fields.includes("password") : fields.includes(key);
      if (!used) continue;
    }
    out[key] = v;
  }
  return out;
}

/// Secret edits to send. Secrets of fields the engine does not use, and the password while
/// password_command is used, are removed.
export function secretsForSave(
  item: ServerItem,
  templates: TemplateEngines = {},
): Partial<Record<SecretKey, SecretChange>> {
  const value = valueForSave(item, templates);
  const out: Partial<Record<SecretKey, SecretChange>> = { ...item.secretEdits };
  const remove = (key: SecretKey) => {
    if (secretWillBeSet(item, key) || item.secretsSet.includes(key)) out[key] = { op: "unset" };
    else delete out[key];
  };
  // password_command and password cannot be combined, and some engines have no password
  const usesPassword =
    (!engineChanged(item, templates) || fieldsFor(effectiveEngine(value, templates)).includes("password")) &&
    typeof value.password_command !== "string";
  if (!usesPassword) remove("password");
  // No tunnel, or the system ssh (ssh_config) handles authentication
  const tunnel = obj(value.ssh_tunnel);
  if (!("ssh_tunnel" in value) || typeof tunnel.ssh_config === "string") {
    remove("ssh_tunnel.password");
    remove("ssh_tunnel.private_key_passphrase");
  }
  return out;
}

export function serverEdit(item: ServerItem, templates: TemplateEngines = {}) {
  return {
    origin: item.origin,
    copy_secrets_from: item.copySecretsFrom,
    value: valueForSave(item, templates),
    secrets: secretsForSave(item, templates),
  };
}

/// The save request (connections.rs `ConnectionsPatch`)
export function buildPatch(items: Item[], revision: string, templates: TemplateEngines = {}) {
  return {
    revision,
    items: items.map((item) =>
      item.kind === "group"
        ? {
            kind: "group",
            group_name: item.groupName,
            color: item.color || null,
            servers: item.servers.map((s) => serverEdit(s, templates)),
          }
        : { kind: "server", ...serverEdit(item, templates) },
    ),
  };
}

/// Problems the form can show before sending (the backend checks again)
export function validateItems(items: Item[]): string | null {
  const names = new Set<string>();
  for (const item of items) {
    if (item.kind === "group" && item.groupName.trim() === "") return "A group needs a name";
  }
  for (const server of allServers(items)) {
    const name = String(server.value.name ?? "").trim();
    if (name === "") return "Every connection needs a name";
    if (names.has(name)) return `Two connections are named "${name}"`;
    names.add(name);
    if (!server.value.template && String(server.value.engine ?? "").trim() === "") {
      // (an engine inherited from a template is fine)
      return `${name}: choose an engine`;
    }
    const port = server.value.port;
    if (port !== undefined && port !== null && port !== "" && !(Number.isInteger(port) && (port as number) > 0 && (port as number) < 65536)) {
      return `${name}: the port must be 1-65535`;
    }
    const command = server.value.password_command;
    if (typeof command === "string" && command.trim() === "") return `${name}: enter the password command`;
    const tunnel = obj(server.value.ssh_tunnel);
    if ("ssh_tunnel" in server.value && fieldsFor(String(server.value.engine ?? "")).includes("ssh_tunnel")) {
      const alias = typeof tunnel.ssh_config === "string" ? tunnel.ssh_config.trim() : null;
      if (alias === "") return `${name}: enter the Host alias from ~/.ssh/config`;
      if (alias === null && String(tunnel.host ?? "").trim() === "") return `${name}: enter the SSH host`;
    }
  }
  return null;
}
