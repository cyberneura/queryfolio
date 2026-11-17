//! The connection editor (CYBERNEURA-DEV-1299): add / edit / duplicate / delete entries of
//! `servers` in config.yml from a form.
//!
//! The rules (see the "Connection editor" section of AGENTS.md):
//!
//! - **Secrets never reach the webview.** `password`, `ssh_tunnel.password` and
//!   `ssh_tunnel.private_key_passphrase` are removed from what the form gets (only whether they
//!   are set). A save names, per server, where it came from (`origin`, the index path in the file
//!   it was read from) and the secret changes (`set` / `unset`); untouched secrets are copied from
//!   the origin entry on the backend.
//! - **The save replaces `servers` as a whole**, from the structure the form sends, so the file
//!   is rewritten (comments are confirmed once, `.bak` before the first overwrite: the same write
//!   path as the Settings window, `settings::save_doc_change`). Groups (`group_name` + nested
//!   `servers`) are kept as groups. Keys the form does not know are kept (they travel in the value).
//! - **A stale form is refused.** The form carries the `revision` (a hash of the local `servers`
//!   it was built from); if the file changed since, nothing is written (the origins would point
//!   at different entries).
//! - The whole new list is checked with the same parser the app uses (`parse_server_entries`,
//!   template expansion included) plus unique names and `password_command` rules before writing.
//! - When `config_override_command` provides `servers`, the local list has no effect, so the
//!   editor is read-only (decided by the caller).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::config::ServerConfig;
use crate::error::AppError;
use crate::settings::Change;

/// Keys whose values never go to the webview.
pub const SECRET_KEYS: [&str; 3] = [
    "password",
    "ssh_tunnel.password",
    "ssh_tunnel.private_key_passphrase",
];

/// The local `servers` as the editor shows it.
#[derive(Debug, Serialize, PartialEq)]
pub struct LocalServers {
    /// The list with the secret values removed (groups keep their nested `servers`)
    pub servers: serde_json::Value,
    /// Index path (`"3"`, or `"2.1"` inside a group) → secret keys that are set
    pub secrets: BTreeMap<String, Vec<String>>,
    /// Hash of the local config this view was built from (see `ConnectionsPatch::revision`)
    pub revision: String,
}

/// One server in a save request.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerEdit {
    /// Index path of the entry this one was read from (`None` for a new or duplicated entry,
    /// which then copies no secrets unless `copy_secrets_from` says so)
    #[serde(default)]
    pub origin: Option<String>,
    /// Duplicates copy the secrets of the entry they were made from
    #[serde(default)]
    pub copy_secrets_from: Option<String>,
    /// The entry without secrets
    pub value: serde_json::Value,
    /// Secret changes by key (see `SECRET_KEYS`). Absent = keep what the source entry had
    #[serde(default)]
    pub secrets: BTreeMap<String, Change<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemEdit {
    Server(ServerEdit),
    Group {
        group_name: String,
        #[serde(default)]
        color: Option<String>,
        servers: Vec<ServerEdit>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectionsPatch {
    pub revision: String,
    pub items: Vec<ItemEdit>,
}

fn local_servers_seq(doc: &Mapping) -> Result<Vec<Value>, AppError> {
    match doc.get("servers") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(seq)) => Ok(seq.clone()),
        Some(_) => Err(AppError::Config(
            "servers in config.yml is not a list; fix it with Edit config.yml".into(),
        )),
    }
}

/// Hash of the whole local config's structure (secrets included, so a changed password also
/// counts; a change to `server_templates` or `config_override_command` counts too).
pub fn revision_of(doc: &Mapping) -> String {
    let text = serde_yaml::to_string(&Value::Mapping(doc.clone())).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn get_path<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    match key.split_once('.') {
        Some((parent, leaf)) => map.get(parent)?.as_mapping()?.get(leaf),
        None => map.get(key),
    }
}

fn remove_path(map: &mut Mapping, key: &str) -> Option<Value> {
    match key.split_once('.') {
        Some((parent, leaf)) => map.get_mut(parent)?.as_mapping_mut()?.remove(leaf),
        None => map.remove(key),
    }
}

fn is_set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// Removes the secrets from one server entry and returns which were set.
fn strip_secrets(entry: &mut Value) -> Vec<String> {
    let Some(map) = entry.as_mapping_mut() else {
        return Vec::new();
    };
    let mut set = Vec::new();
    for key in SECRET_KEYS {
        if let Some(value) = remove_path(map, key) {
            if is_set(&value) {
                set.push(key.to_string());
            }
        }
    }
    set
}

fn is_group(entry: &Value) -> bool {
    entry
        .as_mapping()
        .is_some_and(|m| m.contains_key("servers"))
}

/// The local `servers` for the editor, without secret values.
pub fn read_local_servers(doc: &Mapping) -> Result<LocalServers, AppError> {
    let original = local_servers_seq(doc)?;
    let mut servers = original.clone();
    let mut secrets = BTreeMap::new();
    for (i, entry) in servers.iter_mut().enumerate() {
        if is_group(entry) {
            if let Some(Value::Sequence(children)) =
                entry.as_mapping_mut().and_then(|m| m.get_mut("servers"))
            {
                for (j, child) in children.iter_mut().enumerate() {
                    let set = strip_secrets(child);
                    if !set.is_empty() {
                        secrets.insert(format!("{i}.{j}"), set);
                    }
                }
            }
        } else {
            let set = strip_secrets(entry);
            if !set.is_empty() {
                secrets.insert(i.to_string(), set);
            }
        }
    }
    Ok(LocalServers {
        servers: serde_json::to_value(Value::Sequence(servers))
            .map_err(|e| AppError::Config(e.to_string()))?,
        secrets,
        revision: revision_of(doc),
    })
}

fn entry_at<'a>(servers: &'a [Value], path: &str) -> Option<&'a Value> {
    match path.split_once('.') {
        Some((i, j)) => {
            let group = servers.get(i.parse::<usize>().ok()?)?;
            group
                .as_mapping()?
                .get("servers")?
                .as_sequence()?
                .get(j.parse::<usize>().ok()?)
        }
        None => servers.get(path.parse::<usize>().ok()?),
    }
}

/// JSON → YAML for an entry, dropping empty strings and empty mappings (strings are not trimmed) (the form removes a key
/// when a field is cleared, so an emptied field = key absent). **Nulls are kept**: the form never
/// sends one of its own, so a null came from the file, where it cancels a value a template would
/// supply (`password: null`, `password_command: null`, ...).
fn clean(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, clean(v)))
                .filter(|(_, v)| match v {
                    serde_json::Value::String(s) => !s.is_empty(),
                    serde_json::Value::Object(m) => !m.is_empty(),
                    _ => true,
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(clean).collect())
        }
        // Strings are written as they are: whitespace can be significant in a database name, a
        // user or a file path, and an untouched entry must be written back unchanged
        other => other,
    }
}

/// Key order for a server written from the form. serde_json (without `preserve_order`) sorts
/// object keys, so the order is rebuilt: the keys in the order the source entry had them, then
/// the remaining keys in this order, then anything else.
const SERVER_KEY_ORDER: [&str; 19] = [
    "name",
    "description",
    "template",
    "engine",
    "host",
    "port",
    "schema",
    "user",
    "password",
    "password_command",
    "aws_profile",
    "tls",
    "ssl_mode",
    "ssl_root_cert",
    "ssh_tunnel",
    "readonly",
    "allow_dangerous_statements",
    "folder_name",
    "color",
];

const TUNNEL_KEY_ORDER: [&str; 8] = [
    "ssh_config",
    "host",
    "port",
    "user",
    "password",
    "private_key_path",
    "private_key_passphrase",
    "identity_agent",
];

fn reorder(map: Mapping, source: Option<&Mapping>, order: &[&str]) -> Mapping {
    let mut rest = map;
    let mut out = Mapping::new();
    let mut take = |key: &Value, out: &mut Mapping| {
        if let Some(v) = rest.remove(key) {
            out.insert(key.clone(), v);
        }
    };
    if let Some(source) = source {
        for key in source.keys() {
            take(key, &mut out);
        }
    }
    for key in order {
        take(&Value::String((*key).into()), &mut out);
    }
    for (k, v) in rest {
        out.insert(k, v);
    }
    out
}

/// Builds one server entry for the file: the edited value plus its secrets (changed ones from
/// the request, untouched ones copied from the source entry in `original`).
pub fn build_server(edit: &ServerEdit, original: &[Value]) -> Result<Value, AppError> {
    let value = serde_yaml::to_value(clean(edit.value.clone()))
        .map_err(|e| AppError::Config(e.to_string()))?;
    let Value::Mapping(mut map) = value else {
        return Err(AppError::Config("A connection must be a mapping".into()));
    };
    // The form never holds secret values; ignore any that came in the value
    for key in SECRET_KEYS {
        remove_path(&mut map, key);
    }
    for key in edit.secrets.keys() {
        if !SECRET_KEYS.contains(&key.as_str()) {
            return Err(AppError::Config(format!("{key} is not a secret field")));
        }
    }
    let source = edit
        .origin
        .as_deref()
        .or(edit.copy_secrets_from.as_deref())
        .and_then(|path| entry_at(original, path))
        .and_then(|v| v.as_mapping());
    if edit.origin.is_some() && source.is_none() {
        return Err(AppError::Config(
            "config.yml changed while the editor was open; reopen it".into(),
        ));
    }
    for key in SECRET_KEYS {
        let secret = match edit.secrets.get(key) {
            Some(Change::Set(v)) => {
                if v.is_empty() || v.contains(['\n', '\r', '\0']) {
                    return Err(AppError::Config(format!(
                        "{key} must be a single line (use Remove to clear it)"
                    )));
                }
                Value::String(v.clone())
            }
            // Removing the password of a templated entry must cancel the template's password
            // (`password: null`), or template expansion would bring it back. Tunnel secrets live
            // inside `ssh_tunnel`, which the entry replaces as a whole, so they need nothing
            Some(Change::Unset) if key == "password" && map.contains_key("template") => Value::Null,
            Some(Change::Unset) => continue,
            // Untouched: the source entry's value exactly as it was, an empty string or an
            // explicit null (which cancels a template's value) included
            None => match source.and_then(|s| get_path(s, key)) {
                Some(value) => value.clone(),
                None => continue,
            },
        };
        match key.split_once('.') {
            Some((parent, leaf)) => {
                // The tunnel was removed in the form: its secrets go with it
                let Some(Value::Mapping(tunnel)) = map.get_mut(parent) else {
                    continue;
                };
                tunnel.insert(Value::String(leaf.into()), secret);
            }
            None => {
                map.insert(Value::String(key.into()), secret);
            }
        }
    }
    // A template may supply `password`; with password_command on this entry, cancel it the way
    // docs/settings.md describes (`password: null`), or the two would be combined and rejected
    if map.contains_key("template")
        && map.contains_key("password_command")
        && !map.contains_key("password")
    {
        map.insert("password".into(), Value::Null);
    }
    // ... and the other way round: a literal password cancels a command the template may supply
    if map.contains_key("template")
        && map.get("password").is_some_and(|v| !v.is_null())
        && !map.contains_key("password_command")
    {
        map.insert("password_command".into(), Value::Null);
    }
    if let Some(Value::Mapping(tunnel)) = map.get("ssh_tunnel").cloned() {
        let source_tunnel = source
            .and_then(|s| s.get("ssh_tunnel"))
            .and_then(|v| v.as_mapping());
        map.insert(
            "ssh_tunnel".into(),
            Value::Mapping(reorder(tunnel, source_tunnel, &TUNNEL_KEY_ORDER)),
        );
    }
    Ok(Value::Mapping(reorder(map, source, &SERVER_KEY_ORDER)))
}

/// Builds the new `servers` list from the request.
pub fn build_servers(patch: &ConnectionsPatch, original: &[Value]) -> Result<Vec<Value>, AppError> {
    let mut out = Vec::new();
    for item in &patch.items {
        match item {
            ItemEdit::Server(edit) => out.push(build_server(edit, original)?),
            ItemEdit::Group {
                group_name,
                color,
                servers,
            } => {
                if group_name.trim().is_empty() {
                    return Err(AppError::Config("A group needs a name".into()));
                }
                let mut group = Mapping::new();
                group.insert("group_name".into(), Value::String(group_name.clone()));
                if let Some(color) = color.as_deref().filter(|c| !c.trim().is_empty()) {
                    group.insert("color".into(), Value::String(color.into()));
                }
                let children = servers
                    .iter()
                    .map(|s| build_server(s, original))
                    .collect::<Result<Vec<_>, _>>()?;
                group.insert("servers".into(), Value::Sequence(children));
                out.push(Value::Mapping(group));
            }
        }
    }
    Ok(out)
}

/// Checks the new list the way the app will read it: the parser (templates expanded, groups
/// flattened), unique non-empty names, and the `password_command` rules.
pub fn validate_servers(servers: &[Value], templates: &[Value]) -> Result<(), AppError> {
    let parsed = crate::config::parse_server_entries(servers, templates, "the connection editor")?;
    let mut names = std::collections::HashSet::new();
    for server in &parsed {
        validate_server(server)?;
        if !names.insert(server.name.as_str()) {
            return Err(AppError::Config(format!(
                "Two connections are named \"{}\"",
                server.name
            )));
        }
    }
    Ok(())
}

pub fn validate_server(server: &ServerConfig) -> Result<(), AppError> {
    if server.name.trim().is_empty() {
        return Err(AppError::Config("A connection needs a name".into()));
    }
    crate::db::parse_engine(&server.engine)?;
    server.password_command()?;
    Ok(())
}

/// The file must still be what the editor was built from: the origins point into it, and a test
/// would otherwise send one entry's secrets to another entry's host.
fn check_revision(doc: &Mapping, revision: &str) -> Result<(), AppError> {
    if revision_of(doc) != revision {
        return Err(AppError::Config(
            "config.yml changed while the editor was open; reopen it to see the current connections"
                .into(),
        ));
    }
    Ok(())
}

/// Applies the request to the local config: replaces `servers` (keeping its position).
pub fn apply_connections(
    doc: &mut Mapping,
    patch: &ConnectionsPatch,
    templates: &[Value],
) -> Result<(), AppError> {
    check_revision(doc, &patch.revision)?;
    let original = local_servers_seq(doc)?;
    let servers = build_servers(patch, &original)?;
    validate_servers(&servers, templates)?;
    match doc.get_mut("servers") {
        Some(slot) => *slot = Value::Sequence(servers),
        None => {
            doc.insert("servers".into(), Value::Sequence(servers));
        }
    }
    Ok(())
}

/// The server a "Test connection" connects to: the edited entry with its secrets resolved and
/// the template expanded, without saving anything.
pub fn server_for_test(
    edit: &ServerEdit,
    revision: &str,
    doc: &Mapping,
    templates: &[Value],
) -> Result<ServerConfig, AppError> {
    check_revision(doc, revision)?;
    let original = local_servers_seq(doc)?;
    let entry = build_server(edit, &original)?;
    let mut parsed =
        crate::config::parse_server_entries(&[entry], templates, "the connection editor")?;
    let server = parsed
        .pop()
        .ok_or_else(|| AppError::Config("Nothing to test".into()))?;
    validate_server(&server)?;
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Mapping {
        crate::settings::parse_local(text).unwrap()
    }

    const FILE: &str = "servers:\n- name: pg\n  engine: postgres\n  host: db\n  password: s3cret\n  ssh_tunnel:\n    host: bastion\n    user: me\n    private_key_passphrase: pp\n- group_name: Team\n  color: red\n  servers:\n  - name: my\n    engine: mysql\n    password: mpw\n- name: lite\n  engine: sqlite\n  schema: /tmp/a.db\n";

    fn edit(origin: Option<&str>, value: serde_json::Value) -> ServerEdit {
        ServerEdit {
            origin: origin.map(str::to_string),
            copy_secrets_from: None,
            value,
            secrets: BTreeMap::new(),
        }
    }

    fn view(doc: &Mapping) -> LocalServers {
        read_local_servers(doc).unwrap()
    }

    #[test]
    fn secrets_are_removed_from_the_view() {
        let doc = yaml(FILE);
        let local = view(&doc);
        let json = serde_json::to_string(&local).unwrap();
        for secret in ["s3cret", "pp", "mpw"] {
            assert!(!json.contains(secret), "{secret} leaked");
        }
        assert_eq!(
            local.secrets.get("0").unwrap(),
            &vec![
                "password".to_string(),
                "ssh_tunnel.private_key_passphrase".to_string()
            ]
        );
        assert_eq!(
            local.secrets.get("1.0").unwrap(),
            &vec!["password".to_string()]
        );
        assert_eq!(local.servers[1]["group_name"], "Team");
    }

    #[test]
    fn untouched_secrets_are_copied_from_the_origin() {
        let doc = yaml(FILE);
        let local = view(&doc);
        let original = local_servers_seq(&doc).unwrap();
        let mut value = local.servers[0].clone();
        value["host"] = serde_json::json!("db2");
        let built = build_server(&edit(Some("0"), value), &original).unwrap();
        let built = built.as_mapping().unwrap();
        assert_eq!(built.get("host").unwrap(), "db2");
        assert_eq!(built.get("password").unwrap(), "s3cret");
        assert_eq!(
            get_path(built, "ssh_tunnel.private_key_passphrase").unwrap(),
            "pp"
        );
    }

    #[test]
    fn secrets_can_be_set_and_removed_and_go_with_the_tunnel() {
        let doc = yaml(FILE);
        let local = view(&doc);
        let original = local_servers_seq(&doc).unwrap();
        let mut e = edit(Some("0"), local.servers[0].clone());
        e.secrets.insert("password".into(), Change::Unset);
        e.secrets
            .insert("ssh_tunnel.password".into(), Change::Set("tp".into()));
        let built = build_server(&e, &original).unwrap();
        let built = built.as_mapping().unwrap();
        assert!(built.get("password").is_none());
        assert_eq!(get_path(built, "ssh_tunnel.password").unwrap(), "tp");
        // Removing the tunnel in the form drops its secrets
        let mut value = local.servers[0].clone();
        value.as_object_mut().unwrap().remove("ssh_tunnel");
        let built = build_server(&edit(Some("0"), value), &original).unwrap();
        assert!(built.as_mapping().unwrap().get("ssh_tunnel").is_none());
    }

    #[test]
    fn a_new_entry_gets_no_secrets_but_a_duplicate_can_copy_them() {
        let doc = yaml(FILE);
        let local = view(&doc);
        let original = local_servers_seq(&doc).unwrap();
        let mut value = local.servers[0].clone();
        value["name"] = serde_json::json!("pg copy");
        let fresh = build_server(&edit(None, value.clone()), &original).unwrap();
        assert!(fresh.as_mapping().unwrap().get("password").is_none());
        let mut dup = edit(None, value);
        dup.copy_secrets_from = Some("0".into());
        let copied = build_server(&dup, &original).unwrap();
        assert_eq!(
            copied.as_mapping().unwrap().get("password").unwrap(),
            "s3cret"
        );
    }

    #[test]
    fn apply_rewrites_the_list_keeping_groups_and_rejects_a_stale_revision() {
        let mut doc = yaml(FILE);
        let local = view(&doc);
        let s = local.servers.as_array().unwrap();
        let patch = ConnectionsPatch {
            revision: local.revision.clone(),
            items: vec![
                ItemEdit::Group {
                    group_name: "Team".into(),
                    color: Some("red".into()),
                    servers: vec![edit(Some("1.0"), s[1]["servers"][0].clone())],
                },
                ItemEdit::Server(edit(Some("0"), s[0].clone())),
            ],
        };
        apply_connections(&mut doc, &patch, &[]).unwrap();
        let servers = local_servers_seq(&doc).unwrap();
        assert_eq!(servers.len(), 2, "lite was deleted");
        assert_eq!(
            get_path(servers[1].as_mapping().unwrap(), "password").unwrap(),
            "s3cret"
        );
        let group = servers[0].as_mapping().unwrap();
        assert_eq!(group.get("group_name").unwrap(), "Team");
        // The file changed since (here: by this very save): refuse
        let stale = ConnectionsPatch {
            revision: local.revision,
            items: vec![],
        };
        assert!(apply_connections(&mut doc, &stale, &[]).is_err());
    }

    #[test]
    fn validation_catches_duplicates_bad_engines_and_password_conflicts() {
        let dup = vec![
            serde_yaml::from_str::<Value>("{name: a, engine: sqlite, schema: /x}").unwrap(),
            serde_yaml::from_str::<Value>("{name: a, engine: sqlite, schema: /y}").unwrap(),
        ];
        assert!(validate_servers(&dup, &[]).is_err());
        let bad = vec![serde_yaml::from_str::<Value>("{name: a, engine: oracle}").unwrap()];
        assert!(validate_servers(&bad, &[]).is_err());
        let both = vec![serde_yaml::from_str::<Value>(
            "{name: a, engine: postgres, password: x, password_command: op read y}",
        )
        .unwrap()];
        assert!(validate_servers(&both, &[]).is_err());
        let templated = vec![serde_yaml::from_str::<Value>("{name: a, template: t}").unwrap()];
        let templates =
            vec![serde_yaml::from_str::<Value>("{name: t, engine: postgres, host: h}").unwrap()];
        validate_servers(&templated, &templates).unwrap();
    }

    #[test]
    fn test_target_resolves_secrets_and_templates() {
        let doc = yaml(FILE);
        let local = view(&doc);
        let server = server_for_test(
            &edit(Some("0"), local.servers[0].clone()),
            &local.revision,
            &doc,
            &[],
        )
        .unwrap();
        assert_eq!(server.password.as_deref(), Some("s3cret"));
        assert_eq!(server.host.as_deref(), Some("db"));
        // The file changed since the editor opened: a test must not use another entry's secret
        let mut changed = doc.clone();
        changed.insert("default_limit".into(), 1.into());
        assert!(server_for_test(
            &edit(Some("0"), local.servers[0].clone()),
            &local.revision,
            &changed,
            &[]
        )
        .is_err());
    }

    #[test]
    fn an_explicit_null_password_cancels_the_template_password() {
        let templates =
            vec![
                serde_yaml::from_str::<Value>("{name: t, engine: postgres, password: tpw}")
                    .unwrap(),
            ];
        let doc = yaml(
            "servers:\n- name: a\n  template: t\n  password: null\n  password_command: op read x\n",
        );
        let local = view(&doc);
        assert!(
            local.secrets.is_empty(),
            "a null password is not a stored secret"
        );
        let original = local_servers_seq(&doc).unwrap();
        let built = build_server(&edit(Some("0"), local.servers[0].clone()), &original).unwrap();
        assert!(built
            .as_mapping()
            .unwrap()
            .get("password")
            .unwrap()
            .is_null());
        validate_servers(&[built], &templates).unwrap();
        // A new entry that uses a template and a password command gets the null too
        let fresh = build_server(
            &edit(
                None,
                serde_json::json!({"name": "b", "template": "t", "password_command": "op read y"}),
            ),
            &[],
        )
        .unwrap();
        validate_servers(&[fresh], &templates).unwrap();
    }

    #[test]
    fn remove_on_a_templated_entry_cancels_and_untouched_empty_passwords_stay() {
        let templates =
            vec![
                serde_yaml::from_str::<Value>("{name: t, engine: postgres, password: tpw}")
                    .unwrap(),
            ];
        let doc = yaml(
            "servers:\n- name: a\n  template: t\n  password: pw\n- name: b\n  template: t\n  password: ''\n",
        );
        let local = view(&doc);
        let original = local_servers_seq(&doc).unwrap();
        let mut a = edit(Some("0"), local.servers[0].clone());
        a.secrets.insert("password".into(), Change::Unset);
        let built = build_server(&a, &original).unwrap();
        assert!(built
            .as_mapping()
            .unwrap()
            .get("password")
            .unwrap()
            .is_null());
        let b = build_server(&edit(Some("1"), local.servers[1].clone()), &original).unwrap();
        assert_eq!(b.as_mapping().unwrap().get("password").unwrap(), "");
        let parsed = crate::config::parse_server_entries(&[b], &templates, "t").unwrap();
        assert_eq!(parsed[0].password.as_deref(), Some(""));
    }

    #[test]
    fn a_literal_password_cancels_the_template_command() {
        let templates = vec![serde_yaml::from_str::<Value>(
            "{name: t, engine: postgres, password_command: op read x}",
        )
        .unwrap()];
        let mut e = edit(None, serde_json::json!({"name": "a", "template": "t"}));
        e.secrets
            .insert("password".into(), Change::Set("pw".into()));
        let built = build_server(&e, &[]).unwrap();
        assert!(built
            .as_mapping()
            .unwrap()
            .get("password_command")
            .unwrap()
            .is_null());
        validate_servers(&[built], &templates).unwrap();
    }

    #[test]
    fn a_null_override_of_a_template_field_is_kept() {
        let templates = vec![serde_yaml::from_str::<Value>(
            "{name: t, engine: postgres, password_command: op read x}",
        )
        .unwrap()];
        let doc =
            yaml("servers:\n- name: a\n  template: t\n  password_command: null\n  password: pw\n");
        let local = view(&doc);
        let original = local_servers_seq(&doc).unwrap();
        let built = build_server(&edit(Some("0"), local.servers[0].clone()), &original).unwrap();
        let map = built.as_mapping().unwrap();
        assert!(map.get("password_command").unwrap().is_null());
        assert_eq!(map.get("password").unwrap(), "pw");
        validate_servers(&[built], &templates).unwrap();
    }

    #[test]
    fn empty_fields_are_dropped_and_unknown_keys_kept() {
        let built = build_server(
            &edit(
                None,
                serde_json::json!({"name": "x", "engine": "postgres", "host": "", "schema": " reports ", "custom": 1, "ssh_tunnel": {"host": ""}}),
            ),
            &[],
        )
        .unwrap();
        assert_eq!(
            serde_yaml::to_string(&built).unwrap(),
            "name: x\nengine: postgres\nschema: ' reports '\ncustom: 1\n"
        );
    }
}
