//! Joplin item serialization format.
//!
//! Format (BaseItem.serialize / unserialize in packages/lib/models/BaseItem.ts):
//! ```text
//! <title>            (first line; omitted when item has no title)
//! <body...>          (notes only; body joined with \n)
//!                    (blank line separator)
//! id: xxxx
//! type_: 1
//! ...props...
//! ```
//! Parsing walks from the bottom up until the first blank line.

use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::BTreeMap;

pub const TYPE_NOTE: i64 = 1;
pub const TYPE_FOLDER: i64 = 2;
pub const TYPE_SETTING: i64 = 3;
pub const TYPE_RESOURCE: i64 = 4;
pub const TYPE_TAG: i64 = 5;
pub const TYPE_NOTE_TAG: i64 = 6;
pub const TYPE_SEARCH: i64 = 7;
pub const TYPE_MASTER_KEY: i64 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Int,
    Text,
    Numeric,
}

/// Field name and SQL type per synced table, mirroring the SQLite schema in
/// JoplinDatabase.ts (base CREATE TABLE + all migrations up to v54).
pub fn table_fields(model_type: i64) -> Option<&'static [(&'static str, FieldType)]> {
    match model_type {
        TYPE_NOTE => Some(NOTE_FIELDS),
        TYPE_FOLDER => Some(FOLDER_FIELDS),
        TYPE_TAG => Some(TAG_FIELDS),
        TYPE_NOTE_TAG => Some(NOTE_TAG_FIELDS),
        TYPE_RESOURCE => Some(RESOURCE_FIELDS),
        TYPE_MASTER_KEY => Some(MASTER_KEY_FIELDS),
        _ => None,
    }
}

const NOTE_FIELDS: &[(&str, FieldType)] = &[
    ("id", FieldType::Text),
    ("parent_id", FieldType::Text),
    ("title", FieldType::Text),
    ("body", FieldType::Text),
    ("created_time", FieldType::Int),
    ("updated_time", FieldType::Int),
    ("is_conflict", FieldType::Int),
    ("latitude", FieldType::Numeric),
    ("longitude", FieldType::Numeric),
    ("altitude", FieldType::Numeric),
    ("author", FieldType::Text),
    ("source_url", FieldType::Text),
    ("is_todo", FieldType::Int),
    ("todo_due", FieldType::Int),
    ("todo_completed", FieldType::Int),
    ("source", FieldType::Text),
    ("source_application", FieldType::Text),
    ("application_data", FieldType::Text),
    ("order", FieldType::Int),
    ("user_created_time", FieldType::Int),
    ("user_updated_time", FieldType::Int),
    ("encryption_cipher_text", FieldType::Text),
    ("encryption_applied", FieldType::Int),
    ("markup_language", FieldType::Int),
    ("is_shared", FieldType::Int),
    ("share_id", FieldType::Text),
    ("master_key_id", FieldType::Text),
    ("deleted_time", FieldType::Int),
    ("conflict_original_id", FieldType::Text),
    ("extracted_resource_ids", FieldType::Text),
    ("is_locked", FieldType::Int),
    ("user_data", FieldType::Text),
];

const FOLDER_FIELDS: &[(&str, FieldType)] = &[
    ("id", FieldType::Text),
    ("title", FieldType::Text),
    ("created_time", FieldType::Int),
    ("updated_time", FieldType::Int),
    ("user_created_time", FieldType::Int),
    ("user_updated_time", FieldType::Int),
    ("encryption_cipher_text", FieldType::Text),
    ("encryption_applied", FieldType::Int),
    ("parent_id", FieldType::Text),
    ("share_id", FieldType::Text),
    ("master_key_id", FieldType::Text),
    ("icon", FieldType::Text),
    ("is_shared", FieldType::Int),
    ("deleted_time", FieldType::Int),
    ("is_locked", FieldType::Int),
    ("user_data", FieldType::Text),
];

const TAG_FIELDS: &[(&str, FieldType)] = &[
    ("id", FieldType::Text),
    ("title", FieldType::Text),
    ("created_time", FieldType::Int),
    ("updated_time", FieldType::Int),
    ("user_created_time", FieldType::Int),
    ("user_updated_time", FieldType::Int),
    ("encryption_cipher_text", FieldType::Text),
    ("encryption_applied", FieldType::Int),
    ("parent_id", FieldType::Text),
    ("is_shared", FieldType::Int),
    ("user_data", FieldType::Text),
];

const NOTE_TAG_FIELDS: &[(&str, FieldType)] = &[
    ("id", FieldType::Text),
    ("note_id", FieldType::Text),
    ("tag_id", FieldType::Text),
    ("created_time", FieldType::Int),
    ("updated_time", FieldType::Int),
    ("user_created_time", FieldType::Int),
    ("user_updated_time", FieldType::Int),
    ("encryption_cipher_text", FieldType::Text),
    ("encryption_applied", FieldType::Int),
    ("is_shared", FieldType::Int),
];

const RESOURCE_FIELDS: &[(&str, FieldType)] = &[
    ("id", FieldType::Text),
    ("title", FieldType::Text),
    ("mime", FieldType::Text),
    ("filename", FieldType::Text),
    ("created_time", FieldType::Int),
    ("updated_time", FieldType::Int),
    ("user_created_time", FieldType::Int),
    ("user_updated_time", FieldType::Int),
    ("encryption_blob_encrypted", FieldType::Int),
    ("file_extension", FieldType::Text),
    ("encryption_cipher_text", FieldType::Text),
    ("encryption_applied", FieldType::Int),
    ("share_id", FieldType::Text),
    ("master_key_id", FieldType::Text),
    ("size", FieldType::Int),
    ("is_shared", FieldType::Int),
    ("blob_updated_time", FieldType::Int),
    ("fetch_status", FieldType::Int),
    ("fetch_error", FieldType::Text),
    ("ocr_text", FieldType::Text),
    ("ocr_details", FieldType::Text),
    ("ocr_status", FieldType::Int),
    ("ocr_error", FieldType::Text),
    ("ocr_driver_id", FieldType::Int),
    ("is_locked", FieldType::Int),
    ("user_data", FieldType::Text),
];

const MASTER_KEY_FIELDS: &[(&str, FieldType)] = &[
    ("id", FieldType::Text),
    ("created_time", FieldType::Int),
    ("updated_time", FieldType::Int),
    ("source_application", FieldType::Text),
    ("encryption_method", FieldType::Int),
    ("checksum", FieldType::Text),
    ("content", FieldType::Text),
];

/// A parsed sync item. Properties keep their raw string form until converted.
#[derive(Debug, Clone, Default)]
pub struct Item {
    /// type_ value
    pub item_type: i64,
    /// Property name -> converted value (numbers as i64/f64, rest as strings)
    pub props: BTreeMap<String, Value>,
    /// Note body (empty for non-notes)
    pub body: String,
}

fn is_timestamp_field(name: &str) -> bool {
    matches!(
        name,
        "created_time" | "updated_time" | "user_created_time" | "user_updated_time"
    )
}

/// Convert a serialized string prop to its typed value (unserialize_format).
fn unserialize_prop(field_type: i64, name: &str, value: &str) -> Result<Value> {
    if name.ends_with('_') {
        return Ok(Value::String(value.to_string()));
    }
    let fields = table_fields(field_type)
        .ok_or_else(|| Error::Storage(format!("unknown item type {field_type}")))?;
    let ftype = fields
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| *t)
        // Unknown fields are dropped by removeUnknownFields in TS; keep them as strings here
        // and let the storage layer ignore them.
        .unwrap_or(FieldType::Text);

    let mut v = value.to_string();
    if is_timestamp_field(name) {
        // ISO8601 -> ms epoch
        v = if v.is_empty() {
            "0".to_string()
        } else {
            parse_iso8601_ms(&v)?.to_string()
        };
    }
    let out = match ftype {
        FieldType::Int => {
            if v.is_empty() {
                Value::Number(0.into())
            } else {
                let n: f64 = v
                    .parse()
                    .map_err(|_| Error::Storage(format!("bad int field {name}={v}")))?;
                Value::Number((n as i64).into())
            }
        }
        FieldType::Numeric => {
            if v.is_empty() {
                Value::Number(0.into())
            } else {
                let n: f64 = v
                    .parse()
                    .map_err(|_| Error::Storage(format!("bad numeric field {name}={v}")))?;
                serde_json::Number::from_f64(n)
                    .map(Value::Number)
                    .unwrap_or(Value::Number(0.into()))
            }
        }
        FieldType::Text => Value::String(v),
    };
    Ok(out)
}

fn parse_iso8601_ms(s: &str) -> Result<i64> {
    // Format: 2024-01-02T03:04:05.678Z (moment's format 'x' handles this).
    let time = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|e| Error::Storage(format!("bad timestamp {s}: {e}")))?;
    Ok(time.unix_timestamp() * 1000 + time.millisecond() as i64)
}

/// Serialize a value the way Joplin does (serialize_format).
pub fn serialize_prop(name: &str, value: &Value) -> String {
    let raw = match value {
        Value::Null => String::new(),
        Value::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    // Joplin's serialize_format returns '' for a falsy timestamp (0/empty);
    // reproducing that keeps uploaded items byte-compatible.
    if is_timestamp_field(name) {
        if raw.is_empty() {
            return String::new();
        }
        if let Ok(ms) = raw.parse::<i64>() {
            return if ms == 0 {
                String::new()
            } else {
                escape_prop(&format_ms_iso8601(ms))
            };
        }
    }
    if name == "body" {
        return raw;
    }
    escape_prop(&raw)
}

fn format_ms_iso8601(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let t =
        time::OffsetDateTime::from_unix_timestamp(secs).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let year = t.year();
    let month = u8::from(t.month());
    let day = t.day();
    let (h, m, s) = (t.hour(), t.minute(), t.second());
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

fn escape_prop(s: &str) -> String {
    s.replace("\\n", "\\\\n")
        .replace("\\r", "\\\\r")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Serialize an item back to the Joplin sync format (BaseItem.serialize):
///
/// ```text
/// <title>
///
/// <body...>            (notes only)
///
/// id: xxxx
/// type_: 1
/// ...props...
/// ```
///
/// Known types emit the full canonical `table_fields` list in schema order
/// (absent/falsy values as empty strings, exactly like `BaseItem.serialize`)
/// with `type_` last; unknown types emit the props they hold. `title`/`body`
/// are emitted as the leading lines, never as props, and timestamps are
/// formatted as ISO8601 via [`serialize_prop`].
///
/// The result round-trips through [`unserialize`] for notes and folders.
pub fn serialize(item: &Item) -> String {
    let title = item
        .props
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title_present = item.props.contains_key("title");
    let body = if item.item_type == TYPE_NOTE {
        item.body.clone()
    } else {
        String::new()
    };

    let mut props: Vec<String> = Vec::new();
    match table_fields(item.item_type) {
        Some(fields) => {
            // Joplin's BaseItem.serialize iterates `shownKeys = fieldNames +
            // type_` and writes EVERY field, using '' for absent/falsy values
            // (serialize_format). Emitting the full field set (not just the
            // props we happen to hold) is what makes our uploads
            // indistinguishable from a native Joplin client's.
            for (name, _) in fields {
                if *name == "title" || *name == "body" {
                    continue;
                }
                let v = item.props.get(*name).cloned().unwrap_or(Value::Null);
                props.push(format!("{name}: {}", serialize_prop(name, &v)));
            }
        }
        None => {
            for (name, v) in &item.props {
                // title/body are emitted as leading lines; type_ is appended
                // last from item_type, so skip any stored copy to avoid a
                // duplicate line.
                if name == "title" || name == "body" || name == "type_" {
                    continue;
                }
                props.push(format!("{name}: {}", serialize_prop(name, v)));
            }
        }
    }
    // `type_` is not part of the table field list; Joplin appends it last.
    props.push(format!(
        "type_: {}",
        serialize_prop("type_", &Value::from(item.item_type))
    ));

    let mut parts: Vec<String> = Vec::new();
    if title_present {
        parts.push(title);
    }
    if !body.is_empty() {
        parts.push(body);
    }
    parts.push(props.join("\n"));
    parts.join("\n\n")
}

/// E2EE key material for uploading: the unlocked master key hex and its id.
#[derive(Debug, Clone)]
pub struct SyncKey {
    pub key_hex: String,
    pub key_id: String,
}

/// Serialize an item for upload, encrypting it when E2EE is enabled (mirrors
/// `BaseItem.serializeForSync`).
///
/// Without a key the plaintext [`serialize`] is returned. With a key the
/// plaintext is encrypted as a StringV1 blob and stored under
/// `encryption_cipher_text` in an "envelope": only the link/sync keys Joplin
/// keeps in clear (id, parent_id, updated_time, ...) plus
/// `encryption_applied: 1` and the cipher text. The download side's
/// `process_item_text` understands this layout symmetrically.
pub fn serialize_for_sync(item: &Item, key: Option<&SyncKey>) -> Result<String> {
    let Some(key) = key else {
        return Ok(serialize(item));
    };
    let plain = serialize(item);
    let cipher = crate::crypto::encrypt_string_v1(&key.key_hex, &key.key_id, &plain)?;

    // Keys kept unencrypted so other clients can link/sync the item. Mirrors
    // Joplin's `keepKeys` exactly (it includes `type_`; serialize() re-emits
    // `type_` last from `item_type` in any case).
    const KEEP: &[&str] = &[
        "id",
        "note_id",
        "tag_id",
        "parent_id",
        "share_id",
        "updated_time",
        "deleted_time",
        "type_",
        "is_locked",
        "extracted_resource_ids",
    ];
    let mut props: BTreeMap<String, Value> = BTreeMap::new();
    for k in KEEP {
        if let Some(v) = item.props.get(*k) {
            props.insert((*k).to_string(), v.clone());
        }
    }
    props.insert("encryption_applied".to_string(), Value::from(1));
    props.insert("encryption_cipher_text".to_string(), Value::String(cipher));
    let reduced = Item {
        item_type: item.item_type,
        props,
        body: String::new(),
    };
    Ok(serialize(&reduced))
}

/// Unserialize an item blob (BaseItem.unserialize).
pub fn unserialize(content: &str) -> Result<Item> {
    // Serialized blobs end with the last property (no trailing newline), but
    // tolerate trailing blank lines for robustness.
    let trimmed = content.trim_end_matches('\n');
    let lines: Vec<&str> = trimmed.split('\n').collect();
    let mut props: BTreeMap<String, String> = BTreeMap::new();
    let mut body_end: Option<usize> = None;

    for i in (0..lines.len()).rev() {
        let line = lines[i].trim();
        if line.is_empty() {
            body_end = Some(i);
            break;
        }
        let p = match line.find(':') {
            Some(p) => p,
            None => return Err(Error::Storage(format!("invalid property format: {line}"))),
        };
        let key = line[..p].trim().to_string();
        let value = line[p + 1..].trim().to_string();
        props.insert(key, value);
    }

    let type_str = props
        .get("type_")
        .ok_or_else(|| Error::Storage("missing required property: type_".into()))?;
    let item_type: i64 = type_str
        .parse()
        .map_err(|_| Error::Storage(format!("bad type_: {type_str}")))?;

    // Newer Joplin versions sync item types we don't model (13 FOLDER_TAG,
    // 10 REVISION, ...). Keep only the common envelope fields so the item can
    // still be tracked (deletion detection) instead of failing the whole sync.
    let fields = match table_fields(item_type) {
        Some(f) => f,
        None => {
            const GENERIC: &[(&str, FieldType)] = &[
                ("id", FieldType::Text),
                ("type_", FieldType::Text),
                ("parent_id", FieldType::Text),
                ("updated_time", FieldType::Int),
                ("created_time", FieldType::Int),
                ("deleted_time", FieldType::Int),
            ];
            let mut out = BTreeMap::new();
            for (key, value) in &props {
                if GENERIC.iter().any(|(n, _)| n == key) {
                    out.insert(
                        key.clone(),
                        if key == "created_time" || key == "updated_time" || key == "deleted_time" {
                            Value::Number(value.parse::<i64>().unwrap_or(0).into())
                        } else {
                            Value::String(value.clone())
                        },
                    );
                }
            }
            if let Some(id) = out.get("id").and_then(Value::as_str) {
                if !id.is_empty() && !is_valid_item_id(id) {
                    return Err(Error::Storage(format!("invalid item ID format: {id}")));
                }
            }
            return Ok(Item {
                item_type,
                props: out,
                body: String::new(),
            });
        }
    };
    let mut out_props = BTreeMap::new();
    for (key, value) in &props {
        if !fields.iter().any(|(n, _)| n == key) && key != "type_" {
            continue; // removeUnknownFields
        }
        out_props.insert(key.clone(), unserialize_prop(item_type, key, value)?);
    }
    if let Some(id) = out_props.get("id").and_then(Value::as_str) {
        if !id.is_empty() && !is_valid_item_id(id) {
            return Err(Error::Storage(format!("invalid item ID format: {id}")));
        }
    }

    let mut body = String::new();
    if let Some(end) = body_end {
        let mut body_lines: Vec<&str> = lines[..end].to_vec();
        if !body_lines.is_empty() {
            let title = body_lines.remove(0); // title is the first line
                                              // TS unserialize sets output.title from the first body line for all items.
            out_props
                .entry("title".to_string())
                .or_insert_with(|| Value::String(title.to_string()));
            // Skip the blank line between title and body.
            while body_lines.first().is_some_and(|l| l.trim().is_empty()) {
                body_lines.remove(0);
            }
            body = body_lines.join("\n");
        }
    }

    Ok(Item {
        item_type,
        props: out_props,
        body,
    })
}

fn is_valid_item_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// Path used on the sync target for an item (BaseItem.systemPath).
pub fn system_path(item_id: &str) -> String {
    format!("{item_id}.md")
}

/// Extract the item id from a sync-target path (BaseItem.pathToId).
pub fn path_to_id(path: &str) -> String {
    let last = path.split('/').next_back().unwrap_or(path);
    let name = last.split('.').next().unwrap_or(last);
    name.split('-').next_back().unwrap_or(name).to_string()
}

/// True when a path looks like a synced item path (BaseItem.isSystemPath).
pub fn is_system_path(path: &str) -> bool {
    let last = path.split('/').next_back().unwrap_or(path);
    let parts: Vec<&str> = last.split('.').collect();
    parts.len() == 2
        && parts[0].len() == 32
        && parts[1] == "md"
        && parts[0].chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "My note title\n",
        "\n",
        "Hello world\n",
        "second line\n",
        "\n",
        "id: 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n",
        "parent_id: 00000000000000000000000000000001\n",
        "created_time: 2024-01-02T03:04:05.678Z\n",
        "updated_time: 2024-01-02T03:04:05.678Z\n",
        "is_conflict: 0\n",
        "is_todo: 1\n",
        "todo_completed: 1700000000000\n",
        "markup_language: 1\n",
        "type_: 1\n"
    );

    #[test]
    fn unserialize_note() {
        let item = unserialize(SAMPLE).unwrap();
        assert_eq!(item.item_type, TYPE_NOTE);
        assert_eq!(
            item.props["id"],
            Value::String("0f1e2d3c4b5a69788796a5b4c3d2e1f0".into())
        );
        assert_eq!(item.props["is_todo"], Value::Number(1.into()));
        assert_eq!(
            item.props["todo_completed"],
            Value::Number(1700000000000u64.into())
        );
        assert_eq!(item.body, "Hello world\nsecond line");
        assert!(item.props.contains_key("markup_language"));
    }

    #[test]
    fn path_helpers() {
        assert_eq!(
            path_to_id("0f1e2d3c4b5a69788796a5b4c3d2e1f0.md"),
            "0f1e2d3c4b5a69788796a5b4c3d2e1f0"
        );
        assert_eq!(path_to_id(".resource/abc123"), "abc123");
        assert!(is_system_path("0f1e2d3c4b5a69788796a5b4c3d2e1f0.md"));
        assert!(!is_system_path(".resource/abc123"));
        assert!(!is_system_path("locks/1_1_x.json"));
        assert_eq!(system_path("abc"), "abc.md");
    }

    #[test]
    fn unserialize_skips_unknown_fields() {
        let blob =
            "Note\n\nbody\n\nid: 0f1e2d3c4b5a69788796a5b4c3d2e1f0\nsome_unknown: 1\ntype_: 1";
        let item = unserialize(blob).unwrap();
        assert!(!item.props.contains_key("some_unknown"));
        assert_eq!(item.body, "body");
    }

    #[test]
    fn serialize_roundtrips_note() {
        // unserialize -> serialize -> unserialize must preserve body and
        // the identifying/timestamp props.
        let item = unserialize(SAMPLE).unwrap();
        let text = serialize(&item);
        assert!(
            text.contains("id: 0f1e2d3c4b5a69788796a5b4c3d2e1f0"),
            "{text}"
        );
        assert!(text.contains("type_: 1"), "{text}");
        assert!(
            text.starts_with("My note title\n\nHello world\nsecond line\n\n"),
            "{text}"
        );

        let back = unserialize(&text).unwrap();
        assert_eq!(back.item_type, TYPE_NOTE);
        assert_eq!(back.body, item.body);
        assert_eq!(back.props["title"], item.props["title"]);
        assert_eq!(back.props["is_todo"], item.props["is_todo"]);
        assert_eq!(back.props["todo_completed"], item.props["todo_completed"]);
        assert_eq!(back.props["updated_time"], item.props["updated_time"]);
    }

    #[test]
    fn serialize_timestamp_is_iso8601() {
        let item = Item {
            item_type: TYPE_NOTE,
            props: {
                let mut m = BTreeMap::new();
                m.insert(
                    "id".to_string(),
                    Value::from("0123456789abcdef0123456789abcdef"),
                );
                m.insert("title".to_string(), Value::from("t"));
                m.insert(
                    "updated_time".to_string(),
                    Value::from(1_700_000_000_000i64),
                );
                m
            },
            body: "b".into(),
        };
        let text = serialize(&item);
        assert!(
            text.contains("updated_time: 2023-11-14T"),
            "timestamp must be ISO8601: {text}"
        );
    }

    #[test]
    fn serialize_roundtrips_folder() {
        let blob = "Notebook\n\nid: 0123456789abcdef0123456789abcdef\ntype_: 2";
        let item = unserialize(blob).unwrap();
        let text = serialize(&item);
        let back = unserialize(&text).unwrap();
        assert_eq!(back.item_type, TYPE_FOLDER);
        assert_eq!(back.props["title"], item.props["title"]);
        assert_eq!(back.props["id"], item.props["id"]);
    }

    #[test]
    fn serialize_handles_unicode_and_escaping() {
        let item = Item {
            item_type: TYPE_NOTE,
            props: {
                let mut m = BTreeMap::new();
                m.insert(
                    "id".to_string(),
                    Value::from("0123456789abcdef0123456789abcdef"),
                );
                m.insert("title".to_string(), Value::from("标题：测试"));
                m.insert("source_url".to_string(), Value::from("a\nb"));
                m
            },
            body: "第一行\n第二行 :/0123456789abcdef0123456789abcdef".into(),
        };
        let text = serialize(&item);
        let back = unserialize(&text).unwrap();
        assert_eq!(back.props["title"], item.props["title"]);
        assert_eq!(back.body, item.body);
    }

    #[test]
    fn serialize_for_sync_without_key_is_plaintext() {
        let item = unserialize(SAMPLE).unwrap();
        assert_eq!(serialize_for_sync(&item, None).unwrap(), serialize(&item));
    }

    #[test]
    fn serialize_emits_full_joplin_field_set() {
        // A note created locally must serialize to the same field set that
        // Joplin's BaseItem.serialize emits: EVERY schema field (absent ones as
        // empty strings), with `type_` last. This is what makes our uploads
        // indistinguishable from a native client's.
        let item = Item {
            item_type: TYPE_NOTE,
            props: {
                let mut m = BTreeMap::new();
                m.insert(
                    "id".to_string(),
                    Value::from("0123456789abcdef0123456789abcdef"),
                );
                m.insert(
                    "parent_id".to_string(),
                    Value::from("fedcba9876543210fedcba9876543210"),
                );
                m.insert("title".to_string(), Value::from("T"));
                m.insert(
                    "created_time".to_string(),
                    Value::from(1_700_000_000_000i64),
                );
                m.insert(
                    "updated_time".to_string(),
                    Value::from(1_700_000_000_000i64),
                );
                m.insert(
                    "user_created_time".to_string(),
                    Value::from(1_700_000_000_000i64),
                );
                m.insert(
                    "user_updated_time".to_string(),
                    Value::from(1_700_000_000_000i64),
                );
                m.insert("is_todo".to_string(), Value::from(0));
                m.insert("markup_language".to_string(), Value::from(1));
                m
            },
            body: String::new(),
        };
        let text = serialize(&item);
        for (name, _) in table_fields(TYPE_NOTE).unwrap() {
            if *name == "title" || *name == "body" {
                continue;
            }
            assert!(
                text.contains(&format!("\n{name}: ")),
                "serialized note is missing field `{name}`:\n{text}"
            );
        }
        assert!(text.ends_with("type_: 1"), "{text}");
        // And it still round-trips through our own parser.
        let back = unserialize(&text).unwrap();
        assert_eq!(back.props["id"], item.props["id"]);
        assert_eq!(
            back.props["user_created_time"],
            item.props["user_created_time"]
        );
        assert_eq!(back.body, item.body);
    }

    #[test]
    fn serialize_absent_and_zero_timestamps_are_empty() {
        // serialize_format returns '' for falsy timestamps; absent props are
        // written as empty values (not omitted) so the line count matches
        // Joplin's output.
        let item = Item {
            item_type: TYPE_NOTE,
            props: {
                let mut m = BTreeMap::new();
                m.insert(
                    "id".to_string(),
                    Value::from("0123456789abcdef0123456789abcdef"),
                );
                m.insert("title".to_string(), Value::from("t"));
                m
            },
            body: String::new(),
        };
        let text = serialize(&item);
        assert!(text.contains("created_time: \n"), "{text}");
        assert!(text.contains("user_created_time: \n"), "{text}");
        assert!(text.contains("user_updated_time: \n"), "{text}");
    }

    #[test]
    fn serialize_for_sync_encrypts_envelope() {
        use crate::crypto;
        let item = unserialize(SAMPLE).unwrap();
        let key = SyncKey {
            key_hex: "ab".repeat(32),
            key_id: "0123456789abcdef0123456789abcdef".into(),
        };
        let text = serialize_for_sync(&item, Some(&key)).unwrap();
        // The plaintext body/title must not leak.
        assert!(!text.contains("Hello world"), "{text}");
        assert!(!text.contains("My note title"), "{text}");

        let env = unserialize(&text).unwrap();
        assert_eq!(env.props["encryption_applied"], Value::from(1));
        assert_eq!(env.props["id"], item.props["id"]);
        assert_eq!(env.props["updated_time"], item.props["updated_time"]);
        assert!(env.body.is_empty(), "envelope carries no body");

        // Decrypt the cipher text and confirm the inner item round-trips.
        let cipher = env.props["encryption_cipher_text"].as_str().unwrap();
        let header = crypto::decode_header(cipher).unwrap();
        assert_eq!(header.method, crypto::METHOD_STRING_V1);
        let md_size = usize::from_str_radix(&cipher[5..11], 16).unwrap();
        let chunks = crypto::decode_chunks(&cipher[11 + md_size..]).unwrap();
        let mut inner_text = String::new();
        for chunk in chunks {
            let plain = crypto::native_decrypt(&key.key_hex, &chunk, 3).unwrap();
            let units: Vec<u16> = plain
                .chunks_exact(2)
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .collect();
            inner_text.push_str(&String::from_utf16_lossy(&units));
        }
        let inner = unserialize(&inner_text).unwrap();
        assert_eq!(inner.body, item.body);
        assert_eq!(inner.props["title"], item.props["title"]);
        assert_eq!(inner.props["id"], item.props["id"]);
    }

    #[test]
    fn envelope_carries_timestamps_so_joplin_shim_is_safe() {
        // Joplin's Synchronizer runs, for a remote item without
        // user_created_time:  content.user_created_time = content.created_time
        // If created_time is also ABSENT, that yields undefined and
        // BaseModel.filter turns it into NaN -> "SQLITE_CONSTRAINT: NOT NULL
        // constraint failed: notes.user_created_time", which aborts the whole
        // delta step (requiring a client update/repair). Our envelope must
        // always carry the created_time/user_* lines (empty is fine: Joplin
        // parses '' into 0), even for a freshly created note that has no
        // user_created_time prop yet.
        let item = Item {
            item_type: TYPE_NOTE,
            props: {
                let mut m = BTreeMap::new();
                m.insert(
                    "id".to_string(),
                    Value::from("0123456789abcdef0123456789abcdef"),
                );
                m.insert(
                    "parent_id".to_string(),
                    Value::from("fedcba9876543210fedcba9876543210"),
                );
                m.insert("title".to_string(), Value::from("T"));
                m.insert(
                    "created_time".to_string(),
                    Value::from(1_700_000_000_000i64),
                );
                m.insert(
                    "updated_time".to_string(),
                    Value::from(1_700_000_000_000i64),
                );
                m
            },
            body: String::new(),
        };
        let key = SyncKey {
            key_hex: "ab".repeat(32),
            key_id: "0123456789abcdef0123456789abcdef".into(),
        };
        let text = serialize_for_sync(&item, Some(&key)).unwrap();
        assert!(
            text.contains("\ncreated_time: "),
            "envelope must carry a created_time line: {text}"
        );
        assert!(
            text.contains("\nuser_created_time: "),
            "envelope must carry a user_created_time line: {text}"
        );
        assert!(
            text.contains("\nuser_updated_time: "),
            "envelope must carry a user_updated_time line: {text}"
        );
        // And it still parses (empty timestamps -> 0, not an error).
        let env = unserialize(&text).unwrap();
        assert_eq!(env.props["encryption_applied"], Value::from(1));
    }
}
