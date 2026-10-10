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
    if is_timestamp_field(name) && !raw.is_empty() {
        if let Ok(ms) = raw.parse::<i64>() {
            return escape_prop(&format_ms_iso8601(ms));
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

    let fields = table_fields(item_type)
        .ok_or_else(|| Error::Storage(format!("unknown item type {item_type}")))?;
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
}
