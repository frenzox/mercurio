//! How answers are shown: tables for lists, `key: value` lines for the
//! rest, or the data as JSON.

use serde_json::{Map, Value};

use crate::commands::{Request, BROKER_TOPIC};

/// A table column: its heading, and the fields it shows, the first present.
type Column = (&'static str, &'static [&'static str]);

const CLIENTS: &[Column] = &[
    ("CLIENT ID", &["clientid"]),
    ("USERNAME", &["username"]),
    ("CONNECTED", &["connected"]),
    ("ADDRESS", &["address"]),
    ("SINCE", &["connectedSince", "disconnectedSince"]),
    ("SUBS", &["subscriptions"]),
    ("QUEUED", &["queued"]),
    ("INFLIGHT", &["inflight"]),
    ("DROPPED", &["dropped"]),
];
const BANS: &[Column] = &[
    ("TYPE", &["type"]),
    ("VALUE", &["value"]),
    ("PREFIX", &["prefix"]),
    ("CREATED", &["createdAt"]),
    ("EXPIRES", &["expiresAt"]),
    ("REASON", &["reason"]),
];
const RETAINED: &[Column] = &[
    ("TOPIC", &["topic"]),
    ("QOS", &["qos"]),
    ("SIZE", &["size"]),
    ("EXPIRES IN", &["expiresIn"]),
    ("PREVIEW", &["preview"]),
];
const SUBSCRIPTIONS: &[Column] = &[("FILTER", &["filter"]), ("SUBSCRIBERS", &["subscribers"])];
const SUBSCRIBERS: &[Column] = &[
    ("CLIENT ID", &["clientid"]),
    ("QOS", &["qos"]),
    ("NO LOCAL", &["noLocal"]),
    ("RETAIN AS PUBLISHED", &["retainAsPublished"]),
    ("SUBSCRIPTION ID", &["subscriptionIdentifier"]),
];
const LISTENERS: &[Column] = &[
    ("ADDRESS", &["address", "path"]),
    ("PROTOCOL", &["protocol"]),
    ("TLS", &["tls"]),
    ("MOUNT POINT", &["mountPoint"]),
    ("CONNECTIONS", &["connections"]),
];
const BRIDGES: &[Column] = &[
    ("NAME", &["name"]),
    ("CONNECTED", &["connected"]),
    ("REMOTE", &["remoteAddress"]),
    ("SINCE", &["connectedSince", "disconnectedSince"]),
    ("QUEUED", &["queued"]),
];
const PLUGINS: &[Column] = &[
    ("NAME", &["name"]),
    ("KIND", &["kind"]),
    ("HOOKS", &["hooks"]),
    ("INSTANCES", &["instances"]),
    ("FAILURES", &["failures"]),
    ("LAST ERROR", &["lastError"]),
];
const RELOADED: &[Column] = &[
    ("ITEM", &["item"]),
    ("PATH", &["path"]),
    ("OK", &["ok"]),
    ("ERROR", &["error"]),
];

/// What to print for `data`, the answer to `request`.
pub fn render(request: &Request, data: &Value, json: bool) -> String {
    // The configuration is a nested document no table fits
    if json || request.name() == "getConfig" {
        return format!("{data:#}\n");
    }
    let offset = request.command["offset"].as_u64().unwrap_or(0);
    match (request.topic, request.name()) {
        (BROKER_TOPIC, "listClients") => listing(data, "clients", CLIENTS, offset),
        (BROKER_TOPIC, "listBans") => listing(data, "bans", BANS, offset),
        (BROKER_TOPIC, "listRetained") => listing(data, "messages", RETAINED, offset),
        (BROKER_TOPIC, "listSubscriptions") => {
            listing(data, "subscriptions", SUBSCRIPTIONS, offset)
        }
        (BROKER_TOPIC, "getSubscribers") => listing(data, "subscribers", SUBSCRIBERS, offset),
        (BROKER_TOPIC, "listListeners") => listing(data, "listeners", LISTENERS, offset),
        (BROKER_TOPIC, "listBridges") => listing(data, "bridges", BRIDGES, offset),
        (BROKER_TOPIC, "listPlugins") => listing(data, "plugins", PLUGINS, offset),
        (BROKER_TOPIC, "reload") => listing(data, "items", RELOADED, offset),
        (BROKER_TOPIC, "getLogLevel") => format!("{}\n", cell("level", &data["level"])),
        _ => generic(data),
    }
}

/// Whether a `reload` answer reports a file the broker couldn't reload.
pub fn reload_failed(data: &Value) -> bool {
    data["items"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["ok"] == false))
}

fn listing(data: &Value, key: &str, columns: &[Column], offset: u64) -> String {
    let items = data[key].as_array().map_or(&[][..], Vec::as_slice);
    let mut rows: Vec<Vec<String>> = vec![columns
        .iter()
        .map(|(heading, _)| heading.to_string())
        .collect()];
    for item in items {
        rows.push(
            columns
                .iter()
                .map(|(_, fields)| {
                    let field = fields
                        .iter()
                        .copied()
                        .find(|field| item.get(field).is_some())
                        .unwrap_or(fields[0]);
                    cell(field, &item[field])
                })
                .collect(),
        );
    }
    let mut out = aligned(&rows, 0);
    let shown = offset + items.len() as u64;
    if let Some(total) = data["totalCount"].as_u64().filter(|total| shown < *total) {
        out.push_str(&format!(
            "{}-{shown} of {total}; --offset {shown} shows the next ones\n",
            offset + 1
        ));
    }
    out
}

/// Data no table is made for: a lone list as lines or a table, anything
/// else as `key: value` lines.
fn generic(data: &Value) -> String {
    let Value::Object(object) = data else {
        return format!("{}\n", cell("", data));
    };
    let mut members = object.iter().filter(|(key, _)| *key != "totalCount");
    if let (Some((_, Value::Array(items))), None) = (members.next(), members.next()) {
        return items_text(items, 0);
    }
    let mut out = String::new();
    details(object, 0, &mut out);
    out
}

fn details(object: &Map<String, Value>, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    let width = object
        .keys()
        .map(|key| key.chars().count() + 1)
        .max()
        .unwrap_or(0);
    for (key, value) in object {
        match value {
            Value::Object(inner) => {
                out.push_str(&format!("{pad}{key}:\n"));
                details(inner, indent + 2, out);
            }
            Value::Array(items) if items.iter().any(Value::is_object) => {
                out.push_str(&format!("{pad}{key}:\n"));
                out.push_str(&items_text(items, indent + 2));
            }
            _ => out.push_str(&format!(
                "{pad}{:<width$}  {}\n",
                format!("{key}:"),
                cell(key, value)
            )),
        }
    }
}

/// Scalars one per line; objects as a table of every field any of them has.
fn items_text(items: &[Value], indent: usize) -> String {
    if !items.iter().any(Value::is_object) {
        return items
            .iter()
            .map(|item| format!("{}{}\n", " ".repeat(indent), cell("", item)))
            .collect();
    }
    let mut fields: Vec<&str> = Vec::new();
    for item in items.iter().filter_map(Value::as_object) {
        for field in item.keys() {
            if !fields.contains(&field.as_str()) {
                fields.push(field);
            }
        }
    }
    let mut rows: Vec<Vec<String>> = vec![fields.iter().copied().map(heading).collect()];
    rows.extend(items.iter().map(|item| {
        fields
            .iter()
            .map(|field| cell(field, &item[*field]))
            .collect()
    }));
    aligned(&rows, indent)
}

/// `rows` in columns two spaces apart, each line indented by `indent`.
fn aligned(rows: &[Vec<String>], indent: usize) -> String {
    let mut widths = vec![0; rows.first().map_or(0, Vec::len)];
    for row in rows {
        for (width, text) in widths.iter_mut().zip(row) {
            *width = (*width).max(text.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(text, width)| format!("{text:<width$}"))
            .collect();
        out.push_str(&" ".repeat(indent));
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// One value as text: `-` for none, and the Unix seconds of fields named
/// `…At` or `…Since` as UTC.
fn cell(field: &str, value: &Value) -> String {
    match value {
        Value::Null => "-".to_string(),
        // A line break in a payload preview would break the table
        Value::String(text) => text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
        Value::Array(items) if items.is_empty() => "-".to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| cell("", item))
            .collect::<Vec<_>>()
            .join(","),
        Value::Number(n) if field.ends_with("At") || field.ends_with("Since") => {
            n.as_u64().map_or_else(|| n.to_string(), utc)
        }
        other => other.to_string(),
    }
}

/// `connectedSince` as `CONNECTED SINCE`.
fn heading(field: &str) -> String {
    let mut out = String::new();
    for c in field.chars() {
        if c.is_uppercase() && !out.is_empty() {
            out.push(' ');
        }
        out.extend(c.to_uppercase());
    }
    out
}

/// Unix seconds as a UTC date and time, with Howard Hinnant's
/// days-to-civil algorithm: days are shifted to start on 0000-03-01 so leap
/// days fall at the end of each year, split into 400-year eras of 146097
/// days, and months counted from March in 153-day blocks of five.
fn utc(secs: u64) -> String {
    let (days, time) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        time / 3_600,
        time / 60 % 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{reload_failed, render, utc};
    use crate::commands::{Request, BROKER_TOPIC, DYNSEC_TOPIC};

    fn shown(command: Value, data: Value) -> String {
        let request = Request {
            topic: BROKER_TOPIC,
            command,
        };
        render(&request, &data, false)
    }

    #[test]
    fn unix_seconds_print_as_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00Z");
        assert_eq!(utc(1_794_340_800), "2026-11-10 20:00:00Z");
    }

    #[test]
    fn clients_print_as_a_table() {
        let data = json!({
            "clients": [
                {
                    "clientid": "sensor-1", "username": "alice", "connected": true,
                    "address": "10.0.0.7:51234", "listener": "0.0.0.0:1883",
                    "protocolVersion": 5, "connectedSince": 1794340800, "subscriptions": 2,
                    "queued": 0, "inflight": 0, "dropped": 0,
                },
                {
                    "clientid": "s2", "username": null, "connected": false, "address": null,
                    "listener": null, "protocolVersion": 4, "disconnectedSince": 1794344400,
                    "subscriptions": 0, "queued": 12, "inflight": 0, "dropped": 7,
                },
            ],
            "totalCount": 2,
        });
        assert_eq!(
            shown(json!({"command": "listClients"}), data),
            "\
CLIENT ID  USERNAME  CONNECTED  ADDRESS         SINCE                 SUBS  QUEUED  INFLIGHT  DROPPED
sensor-1   alice     true       10.0.0.7:51234  2026-11-10 20:00:00Z  2     0       0         0
s2         -         false      -               2026-11-10 21:00:00Z  0     12      0         7
"
        );
    }

    #[test]
    fn a_partial_list_says_how_to_get_the_rest() {
        let data = json!({
            "bans": [{
                "type": "clientid", "value": "evil", "prefix": false, "reason": null,
                "createdAt": 1794340800, "expiresAt": null,
            }],
            "totalCount": 250,
        });
        let text = shown(
            json!({"command": "listBans", "offset": 100, "count": 1}),
            data,
        );
        assert!(
            text.ends_with("101-101 of 250; --offset 101 shows the next ones\n"),
            "{text}"
        );
    }

    #[test]
    fn reload_results_print_as_a_table() {
        let data = json!({ "items": [
            { "item": "password-file", "path": "/etc/mercurio/passwd", "ok": true },
            {
                "item": "acl-file", "path": "/etc/mercurio/acl", "ok": false,
                "error": "syntax error at line 3",
            },
        ] });
        assert_eq!(
            shown(json!({"command": "reload"}), data.clone()),
            "\
ITEM           PATH                  OK     ERROR
password-file  /etc/mercurio/passwd  true   -
acl-file       /etc/mercurio/acl     false  syntax error at line 3
"
        );
        assert!(reload_failed(&data));
        assert!(!reload_failed(
            &json!({ "items": [{ "item": "a", "ok": true }] })
        ));
    }

    #[test]
    fn other_answers_print_as_key_value_lines() {
        let data = json!({
            "version": "0.4.1", "startedAt": 1794340800, "uptime": 3600, "storage": "sqlite",
            "stats": { "clients/connected": 3, "messages/received": 120 },
        });
        assert_eq!(
            shown(json!({"command": "getBrokerInfo"}), data),
            "\
startedAt:  2026-11-10 20:00:00Z
stats:
  clients/connected:  3
  messages/received:  120
storage:    sqlite
uptime:     3600
version:    0.4.1
"
        );
        assert_eq!(
            shown(
                json!({"command": "deleteRetained"}),
                json!({ "deleted": 3 })
            ),
            "deleted:  3\n"
        );
    }

    #[test]
    fn the_log_level_prints_alone() {
        assert_eq!(
            shown(
                json!({"command": "getLogLevel"}),
                json!({ "level": "info" })
            ),
            "info\n"
        );
    }

    #[test]
    fn json_prints_the_data_and_the_configuration_is_always_json() {
        let request = Request {
            topic: BROKER_TOPIC,
            command: json!({"command": "listClients"}),
        };
        let data = json!({ "clients": [], "totalCount": 0 });
        assert_eq!(render(&request, &data, true), format!("{data:#}\n"));
        let config = json!({ "storage": { "backend": "memory" } });
        assert_eq!(
            shown(json!({"command": "getConfig"}), config.clone()),
            format!("{config:#}\n")
        );
    }

    #[test]
    fn dynamic_security_answers_print_generically() {
        let dynsec = |command: Value, data: Value| {
            let request = Request {
                topic: DYNSEC_TOPIC,
                command,
            };
            render(&request, &data, false)
        };
        assert_eq!(
            dynsec(
                json!({"command": "listClients"}),
                json!({ "totalCount": 2, "clients": ["alice", "bob"] })
            ),
            "alice\nbob\n"
        );
        assert_eq!(
            dynsec(
                json!({"command": "getClient"}),
                json!({ "client": {
                    "username": "alice", "disabled": false,
                    "roles": [{ "rolename": "sensors" }], "groups": [],
                } })
            ),
            "\
client:
  disabled:  false
  groups:    -
  roles:
    ROLENAME
    sensors
  username:  alice
"
        );
    }
}
