import { test } from "node:test";
import assert from "node:assert/strict";
import {
  buildPatch,
  duplicateServer,
  itemsFromView,
  newServer,
  schemaLabel,
  secretsForSave,
  validateItems,
  valueForSave,
  type ServerItem,
} from "../src/lib/connectionsForm.ts";

const local = {
  servers: [
    { name: "pg", engine: "postgres", host: "db", custom: 1, ssh_tunnel: { host: "b", user: "u" } },
    { group_name: "Team", color: "red", servers: [{ name: "lite", engine: "sqlite", schema: "/a.db" }] },
  ],
  secrets: { "0": ["password", "ssh_tunnel.private_key_passphrase"] },
  revision: "r1",
};

test("items keep groups, origins and secret flags", () => {
  const items = itemsFromView(local);
  assert.equal(items[0].kind, "server");
  assert.equal((items[0] as ServerItem).origin, "0");
  assert.deepEqual((items[0] as ServerItem).secretsSet, ["password", "ssh_tunnel.private_key_passphrase"]);
  assert.equal(items[1].kind, "group");
  if (items[1].kind === "group") assert.equal(items[1].servers[0].origin, "1.0");
});

test("an untouched list round-trips into the same request", () => {
  const patch = buildPatch(itemsFromView(local), "r1");
  assert.deepEqual(patch, {
    revision: "r1",
    items: [
      { kind: "server", origin: "0", copy_secrets_from: null, value: local.servers[0], secrets: {} },
      {
        kind: "group",
        group_name: "Team",
        color: "red",
        servers: [{ origin: "1.0", copy_secrets_from: null, value: local.servers[1].servers[0], secrets: {} }],
      },
    ],
  });
});

test("switching the engine drops fields and secrets it does not use, keeping unknown keys", () => {
  const item = itemsFromView(local)[0] as ServerItem;
  item.value.engine = "sqlite";
  item.value.schema = "/x.db";
  assert.deepEqual(valueForSave(item), { name: "pg", engine: "sqlite", custom: 1, schema: "/x.db" });
  // only the secrets that are set get an unset
  assert.deepEqual(secretsForSave(item), {
    password: { op: "unset" },
    "ssh_tunnel.private_key_passphrase": { op: "unset" },
  });
});

test("password_command replaces the stored password; ssh_config drops the tunnel secrets", () => {
  const item = itemsFromView(local)[0] as ServerItem;
  item.value.password_command = 'op read "op://v/i/password"';
  item.value.ssh_tunnel = { ssh_config: "bastion-alias" };
  const secrets = secretsForSave(item);
  assert.deepEqual(secrets.password, { op: "unset" });
  assert.deepEqual(secrets["ssh_tunnel.private_key_passphrase"], { op: "unset" });
  assert.equal(valueForSave(item).password_command, 'op read "op://v/i/password"');
});

test("duplicates get a fresh name and copy secrets from the source", () => {
  const item = itemsFromView(local)[0] as ServerItem;
  const copy = duplicateServer(item, ["pg", "pg copy"]);
  assert.equal(copy.value.name, "pg copy 2");
  assert.equal(copy.origin, null);
  assert.equal(copy.copySecretsFrom, "0");
});

test("validation", () => {
  const items = itemsFromView(local);
  assert.equal(validateItems(items), null);
  const fresh = newServer();
  assert.ok(validateItems([...items, fresh])?.includes("name"));
  fresh.value.name = "pg";
  assert.ok(validateItems([...items, fresh])?.includes("Two connections"));
  fresh.value.name = "pg2";
  fresh.value.port = 70000;
  assert.ok(validateItems([...items, fresh])?.includes("port"));
  assert.equal(schemaLabel("sqlite3"), "Database file");
  assert.equal(schemaLabel("dynamodb"), "AWS region");
});

test("an entry whose engine did not change keeps every key, even ones the form does not show", () => {
  const items = itemsFromView({
    servers: [
      { name: "pg", engine: "postgres", tls: true, host: "db" },
      { name: "old-lite", engine: "sqlite", host: "/data/a.db" },
      { name: "dyn", template: "aws" },
    ],
    secrets: {},
    revision: "r",
  }, { aws: "dynamodb" });
  const [pg, lite, dyn] = items as ServerItem[];
  pg.value.host = "db2";
  assert.deepEqual(valueForSave(pg), { name: "pg", engine: "postgres", tls: true, host: "db2" });
  assert.deepEqual(valueForSave(lite), { name: "old-lite", engine: "sqlite", host: "/data/a.db" });
  dyn.value.aws_profile = "staging";
  assert.deepEqual(valueForSave(dyn, { aws: "dynamodb" }), { name: "dyn", template: "aws", aws_profile: "staging" });
});

test("ssh_tunnel: null (cancelling a template's tunnel) passes validation", () => {
  const items = itemsFromView({ servers: [{ name: "a", template: "t", ssh_tunnel: null }], secrets: {}, revision: "r" }, { t: "postgres" });
  assert.equal(validateItems(items), null);
});
