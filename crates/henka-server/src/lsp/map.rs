//! Translating between LSP messages and Henka's operation model.
//!
//! Requests carry a `textDocument`/`position`; the operations answer with
//! Henka's own structured JSON (paths relative to the project root, each
//! location quoting the source it points at). These helpers build operation
//! targets from LSP request params, and reshape operation results into the LSP
//! responses an editor expects — carrying Henka's `text`/`context` source
//! enrichment through as extra fields on each location, so a model reading the
//! response sees the code the way it does from the MCP surface (and from grep),
//! not a bare coordinate.

use std::path::Path;

use henka_core::{Position, Target};
use henka_lsp::{path_to_file_uri, uri_to_path};
use serde_json::{Value, json};

/// The identity a workspace folder resolves to: the base project, and — when
/// the folder is a named working copy rather than the base checkout — the
/// working copy an edit lands in.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The registered project id: the folder's basename up to the first `.`.
    pub project_id: String,
    /// The opened working copy's path, as the client named it. Always set; used
    /// to build result URIs the client can resolve.
    pub folder: std::path::PathBuf,
    /// The working copy to target, when the folder names one distinct from the
    /// base checkout (a basename with a `.` suffix, e.g. `proj.workspace`).
    /// `None` for the base project, so operations use the project root and take
    /// the on-base fast path instead of overlaying it onto itself.
    pub workspace: Option<std::path::PathBuf>,
}

/// Derive the project/workspace identity from an `initialize` request's
/// `workspaceFolders` (or the deprecated `rootUri`/`rootPath`).
pub fn identity(params: &Value) -> Option<Identity> {
    let uri = params
        .get("workspaceFolders")
        .and_then(Value::as_array)
        .and_then(|folders| folders.first())
        .and_then(|f| f.get("uri"))
        .and_then(Value::as_str)
        .or_else(|| params.get("rootUri").and_then(Value::as_str));
    let folder = match uri {
        Some(uri) => uri_to_path(uri),
        None => std::path::PathBuf::from(params.get("rootPath").and_then(Value::as_str)?),
    };
    let name = folder.file_name()?.to_str()?;
    // A `.` suffix names a working copy under the base project; no suffix is the
    // base checkout itself.
    let (project_id, workspace) = match name.split_once('.') {
        Some((base, _)) => (base.to_string(), Some(folder.clone())),
        None => (name.to_string(), None),
    };
    (!project_id.is_empty()).then_some(Identity {
        project_id,
        folder,
        workspace,
    })
}

/// Build a position target from a request's `textDocument.uri` and `position`.
pub fn position_target(params: &Value) -> Result<Target, String> {
    let file = uri_to_path(text_document_uri(params)?);
    let pos = params
        .get("position")
        .ok_or("missing `position`")?;
    let line = pos.get("line").and_then(Value::as_u64).ok_or("missing `position.line`")? as u32;
    let character = pos
        .get("character")
        .and_then(Value::as_u64)
        .ok_or("missing `position.character`")? as u32;
    Ok(Target::Position {
        file,
        position: Position::new(line, character),
    })
}

/// Build a whole-file target from a request's `textDocument.uri`.
pub fn file_target(params: &Value) -> Result<Target, String> {
    Ok(Target::File {
        file: uri_to_path(text_document_uri(params)?),
    })
}

/// The `textDocument.uri` string from a request's params.
pub fn text_document_uri(params: &Value) -> Result<&str, String> {
    params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `textDocument.uri`".to_string())
}

/// Reshape a location-list query result (`find-usages`, `go-to-definition`,
/// `find-implementations`) into an LSP `Location[]`, keeping each location's
/// source text as extra fields.
pub fn locations(query: &Value, key: &str, folder: &Path) -> Value {
    let items = query
        .get(key)
        .and_then(Value::as_array)
        .map(|entries| entries.iter().map(|e| location(e, folder)).collect())
        .unwrap_or_default();
    Value::Array(items)
}

/// One LSP `Location`, augmented with Henka's source enrichment.
fn location(entry: &Value, folder: &Path) -> Value {
    let mut out = json!({
        "uri": uri_of(folder, entry),
        "range": range_of(entry),
    });
    carry_context(entry, &mut out);
    out
}

/// Reshape a `describe-symbol` result into an LSP `Hover`, or `null` when there
/// is nothing to say.
pub fn hover(query: &Value) -> Value {
    let text = query.get("text").and_then(Value::as_str).unwrap_or_default();
    if text.is_empty() {
        return Value::Null;
    }
    let mut out = json!({ "contents": { "kind": "markdown", "value": text } });
    if query.get("start_line").is_some() {
        out["range"] = range_of(query);
    }
    out
}

/// Reshape a `file-outline` result into an LSP `DocumentSymbol[]`. `kind` is
/// kept as Henka's readable name rather than the LSP integer, and each symbol
/// carries its declaration source as extra fields.
pub fn outline(query: &Value) -> Value {
    symbols(query.get("symbols"))
}

fn symbols(list: Option<&Value>) -> Value {
    let items = list
        .and_then(Value::as_array)
        .map(|entries| entries.iter().map(document_symbol).collect())
        .unwrap_or_default();
    Value::Array(items)
}

fn document_symbol(entry: &Value) -> Value {
    // The declaration's full extent is the LSP `range`; the name is the
    // `selectionRange`. Henka reports the name range as the top-level
    // coordinates and the body's line span alongside. LSP requires `range` to
    // contain `selectionRange`, so when the body is a single line (its last line
    // is the name's line) the range must end at the name's end column, not at
    // column 0 — otherwise the range is empty and a strict client drops the
    // symbol.
    let num = |key: &str| entry.get(key).and_then(Value::as_u64).unwrap_or(0);
    let body_start = num("body_start_line");
    let body_end = num("body_end_line");
    let name_end_line = num("end_line");
    let (end_line, end_character) = if body_end > name_end_line {
        (body_end, 0)
    } else {
        (name_end_line, num("end_character"))
    };
    let mut out = json!({
        "name": entry.get("name").cloned().unwrap_or(json!("")),
        "kind": entry.get("kind").cloned().unwrap_or(json!("")),
        "range": {
            "start": { "line": body_start.min(num("start_line")), "character": 0 },
            "end": { "line": end_line, "character": end_character },
        },
        "selectionRange": range_of(entry),
        "children": symbols(entry.get("children")),
    });
    if let Some(detail) = entry.get("detail") {
        out["detail"] = detail.clone();
    }
    carry_context(entry, &mut out);
    out
}

/// Reshape a `prepare-call-hierarchy` result into LSP `CallHierarchyItem[]`.
///
/// Each entry's `item` is the server's own handle, returned verbatim so the
/// client can hand it back on the follow-up call; the source enrichment is added
/// alongside.
pub fn call_hierarchy_items(query: &Value) -> Value {
    let items = query
        .get("items")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(call_hierarchy_item).collect())
        .unwrap_or_default();
    Value::Array(items)
}

fn call_hierarchy_item(entry: &Value) -> Option<Value> {
    let mut item = entry.get("item")?.clone();
    carry_context(entry, &mut item);
    Some(item)
}

/// Reshape an `incoming-calls`/`outgoing-calls` result into LSP
/// `CallHierarchyIncomingCall[]` / `CallHierarchyOutgoingCall[]`. `direction` is
/// the field naming the other end (`from` for incoming, `to` for outgoing).
pub fn calls(query: &Value, direction: &str) -> Value {
    let items = query
        .get("calls")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|call| one_call(call, direction))
                .collect()
        })
        .unwrap_or_default();
    Value::Array(items)
}

fn one_call(call: &Value, direction: &str) -> Option<Value> {
    let other = call.get(direction)?;
    let mut item = other.get("item")?.clone();
    carry_context(other, &mut item);
    // The call sites, each carrying its own line so a model can tell overloaded
    // call sites apart without opening the file.
    let from_ranges: Vec<Value> = call
        .get("ranges")
        .and_then(Value::as_array)
        .map(|ranges| {
            ranges
                .iter()
                .map(|r| {
                    let mut site = range_of(r);
                    carry_context(r, &mut site);
                    site
                })
                .collect()
        })
        .unwrap_or_default();
    Some(json!({ direction: item, "fromRanges": from_ranges }))
}

/// Copy Henka's source-enrichment fields (`text`, and the `context` window when
/// present) from a result entry onto an LSP response object, unchanged.
fn carry_context(entry: &Value, out: &mut Value) {
    for key in ["text", "context", "context_start_line"] {
        if let Some(value) = entry.get(key) {
            out[key] = value.clone();
        }
    }
}

/// Build an LSP `Range` from an entry's flat `start_line`/`start_character`/
/// `end_line`/`end_character` fields.
fn range_of(entry: &Value) -> Value {
    let at = |line: &str, ch: &str| {
        json!({
            "line": entry.get(line).cloned().unwrap_or(json!(0)),
            "character": entry.get(ch).cloned().unwrap_or(json!(0)),
        })
    };
    json!({
        "start": at("start_line", "start_character"),
        "end": at("end_line", "end_character"),
    })
}

/// The `file://` URI for an entry's `file`: joined onto the opened folder when
/// relative (the common in-project case), used as-is when absolute.
fn uri_of(folder: &Path, entry: &Value) -> Value {
    let file = entry.get("file").and_then(Value::as_str).unwrap_or_default();
    let path = Path::new(file);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        folder.join(path)
    };
    json!(path_to_file_uri(&abs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_splits_the_workspace_basename() {
        let params = json!({ "workspaceFolders": [{ "uri": "file:///root/stargate.rewind" }] });
        let id = identity(&params).unwrap();
        assert_eq!(id.project_id, "stargate");
        assert_eq!(id.folder, std::path::PathBuf::from("/root/stargate.rewind"));
        // A dotted folder names a distinct working copy to target.
        assert_eq!(id.workspace, Some(std::path::PathBuf::from("/root/stargate.rewind")));

        let params = json!({ "rootUri": "file:///root/stargate" });
        let id = identity(&params).unwrap();
        assert_eq!(id.project_id, "stargate");
        assert!(id.folder.ends_with("stargate"));
        // The base checkout has no separate workspace, so operations use the
        // project root (and the on-base fast path).
        assert_eq!(id.workspace, None);
    }

    #[test]
    fn locations_carry_uri_range_and_source() {
        let query = json!({
            "usages": [{
                "file": "src/auth.rs",
                "start_line": 41, "start_character": 9,
                "end_line": 41, "end_character": 22,
                "text": "  return validate(token);"
            }]
        });
        let out = locations(&query, "usages", Path::new("/root/svc"));
        let loc = &out[0];
        assert_eq!(loc["uri"], json!("file:///root/svc/src/auth.rs"));
        assert_eq!(loc["range"]["start"]["line"], json!(41));
        assert_eq!(loc["range"]["end"]["character"], json!(22));
        // The source line rides along as an extra field, unchanged.
        assert_eq!(loc["text"], json!("  return validate(token);"));
    }

    #[test]
    fn context_window_is_carried_when_present() {
        let query = json!({
            "definitions": [{
                "file": "a.rs", "start_line": 2, "start_character": 0,
                "end_line": 2, "end_character": 5,
                "text": "three", "context": "two\nthree\nfour", "context_start_line": 1
            }]
        });
        let out = locations(&query, "definitions", Path::new("/p"));
        assert_eq!(out[0]["context"], json!("two\nthree\nfour"));
        assert_eq!(out[0]["context_start_line"], json!(1));
    }

    #[test]
    fn hover_wraps_text_as_markdown_or_is_null() {
        let out = hover(&json!({ "text": "`fn f()`", "start_line": 4, "start_character": 3, "end_line": 4, "end_character": 6 }));
        assert_eq!(out["contents"]["value"], json!("`fn f()`"));
        assert_eq!(out["range"]["start"]["line"], json!(4));
        assert_eq!(hover(&json!({ "text": "" })), Value::Null);
    }

    #[test]
    fn outline_nests_children_and_keeps_kind_names() {
        let query = json!({
            "symbols": [{
                "name": "Foo", "kind": "class",
                "start_line": 2, "start_character": 13, "end_line": 2, "end_character": 16,
                "body_start_line": 2, "body_end_line": 4,
                "text": "class Foo {",
                "children": [{
                    "name": "size", "kind": "method", "detail": "() -> int",
                    "start_line": 3, "start_character": 6, "end_line": 3, "end_character": 10,
                    "body_start_line": 3, "body_end_line": 3, "text": "  int size() {}"
                }]
            }]
        });
        let out = outline(&query);
        let class = &out[0];
        assert_eq!(class["name"], json!("Foo"));
        assert_eq!(class["kind"], json!("class"));
        assert_eq!(class["selectionRange"]["start"]["character"], json!(13));
        assert_eq!(class["range"]["end"]["line"], json!(4));
        assert_eq!(class["text"], json!("class Foo {"));
        let method = &class["children"][0];
        assert_eq!(method["name"], json!("size"));
        assert_eq!(method["detail"], json!("() -> int"));
        // Single-line symbol: `range` must still contain `selectionRange`, so it
        // ends at the name's end column rather than an empty column 0.
        assert_eq!(method["range"]["start"]["line"], json!(3));
        assert_eq!(method["range"]["end"]["line"], json!(3));
        assert_eq!(method["range"]["end"]["character"], json!(10));
        assert_eq!(method["selectionRange"]["end"]["character"], json!(10));
    }

    #[test]
    fn prepare_returns_the_opaque_item_with_source() {
        let query = json!({
            "items": [{
                "name": "run", "kind": "method",
                "text": "  void run() {}",
                "item": { "name": "run", "kind": 6, "uri": "file:///p/Foo.java", "data": { "x": 1 } }
            }]
        });
        let out = call_hierarchy_items(&query);
        let item = &out[0];
        // The server's own handle survives verbatim...
        assert_eq!(item["data"]["x"], json!(1));
        assert_eq!(item["uri"], json!("file:///p/Foo.java"));
        // ...with the source carried alongside.
        assert_eq!(item["text"], json!("  void run() {}"));
    }

    #[test]
    fn calls_map_to_direction_and_from_ranges() {
        let query = json!({
            "calls": [{
                "from": {
                    "name": "go", "text": "  void go() {}",
                    "item": { "name": "go", "kind": 6, "uri": "file:///p/C.java", "data": {} }
                },
                "ranges": [{
                    "file": "C.java", "start_line": 2, "start_character": 4,
                    "end_line": 2, "end_character": 10, "text": "    go();"
                }]
            }]
        });
        let out = calls(&query, "from");
        let call = &out[0];
        // `from` is the opaque item itself (a valid CallHierarchyItem to keep
        // walking from), with the source carried alongside.
        assert_eq!(call["from"]["name"], json!("go"));
        assert_eq!(call["from"]["uri"], json!("file:///p/C.java"));
        assert_eq!(call["from"]["text"], json!("  void go() {}"));
        assert_eq!(call["fromRanges"][0]["start"]["line"], json!(2));
        assert_eq!(call["fromRanges"][0]["text"], json!("    go();"));
    }
}
