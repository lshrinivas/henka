//! Conversions between LSP wire types and henka's JSON tool-call shape.
//!
//! Kept in one module so the position-encoding contract (UTF-16 both sides)
//! and URI ↔ path handling live in one place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tower_lsp_server::lsp_types::{
    CreateFile, DeleteFile, DocumentChangeOperation, DocumentChanges, Location, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, Range, RenameFile, ResourceOp, TextEdit,
    Uri, WorkspaceEdit,
};

use crate::mcp::McpClientError;

/// Strip the `file://` prefix from an LSP URI and percent-decode the path.
///
/// Returns `None` for non-`file://` URIs — those aren't something henka can
/// resolve to a real file, so the caller should reply with an LSP error
/// rather than pass an untranslated URI through.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    // `file:///foo` → `/foo`; `file://host/foo` → `/foo` (proxy is single-host).
    let after_host = match rest.find('/') {
        Some(i) => &rest[i..],
        None => "/",
    };
    let decoded = percent_encoding::percent_decode_str(after_host)
        .decode_utf8()
        .ok()?;
    Some(PathBuf::from(decoded.as_ref()))
}

/// Build a `file://` URI from an absolute path. Encodes the parts that must
/// be percent-encoded (spaces, `?`, `#`) but leaves `/` alone.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let s = path.to_str()?;
    let encoded = percent_encoding::utf8_percent_encode(s, PATH_ENCODE_SET).to_string();
    let s = if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    };
    s.parse::<Uri>().ok()
}

const PATH_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'?')
    .add(b'#')
    .add(b'{')
    .add(b'}');

/// Turn henka's `find-usages` response into an LSP `Vec<Location>`.
///
/// Henka returns `{ "count": N, "usages": [{ "file", "start_line",
/// "start_character", "end_line", "end_character" }, ...] }` with paths
/// relative to the project root. We resolve each against `workspace_path` and
/// wrap it in a `file://` URI.
pub fn usages_to_locations(
    value: serde_json::Value,
    workspace_path: &Path,
) -> Result<Vec<Location>, McpClientError> {
    let usages = value
        .get("usages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::with_capacity(usages.len());
    for usage in usages {
        let Some(file) = usage.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let abs = if Path::new(file).is_absolute() {
            PathBuf::from(file)
        } else {
            workspace_path.join(file)
        };
        let Some(uri) = path_to_uri(&abs) else {
            continue;
        };
        let start = read_position(&usage, "start_line", "start_character");
        let end = read_position(&usage, "end_line", "end_character");
        out.push(Location {
            uri,
            range: Range { start, end },
        });
    }
    Ok(out)
}

fn read_position(v: &serde_json::Value, line_key: &str, char_key: &str) -> Position {
    let line = v.get(line_key).and_then(|n| n.as_u64()).unwrap_or(0) as u32;
    let character = v.get(char_key).and_then(|n| n.as_u64()).unwrap_or(0) as u32;
    Position { line, character }
}

/// Convert henka's structured `WorkspaceEdit` (returned inside dry_run
/// responses since the `Include structured edit coordinates in dry-run
/// responses` change) into an LSP `WorkspaceEdit`.
///
/// All or nothing: anything in the payload the proxy can't translate fails the
/// request. Skipping the bad parts would hand the client a partial refactor —
/// five of six references renamed, with nothing to say the sixth was dropped —
/// which is worse than an error the user can retry.
///
/// The result carries `documentChanges` when the client supports them and
/// `changes` when it doesn't, never both: a client that reads both would apply
/// every edit twice, and the two maps can't be made to disagree safely.
pub fn henka_edit_to_workspace_edit(
    value: &serde_json::Value,
    workspace_path: &Path,
    supports_document_changes: bool,
) -> Result<WorkspaceEdit, McpClientError> {
    // Henka's offsets are only meaningful in the encoding it computed them in.
    // Taking a UTF-8 edit as UTF-16 shifts every position on a line holding
    // non-ASCII text, and the result goes straight into the user's files —
    // exactly what `initialize` refuses a UTF-8-only client for.
    let encoding = value
        .get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("Utf16");
    if !encoding.eq_ignore_ascii_case("utf16") {
        return Err(McpClientError::UntranslatableEdit(format!(
            "henka computed the edit in {encoding} position encoding, but this \
             LSP session is UTF-16; applying it would misplace every edit on a \
             line containing non-ASCII characters"
        )));
    }

    let files = as_array(value, "files")?;
    let file_ops = as_array(value, "file_ops")?;

    if !supports_document_changes && !file_ops.is_empty() {
        return Err(McpClientError::UntranslatableEdit(
            "this refactoring creates, renames or deletes files, which can only \
             be expressed as WorkspaceEdit.documentChanges — and the client did \
             not declare support for them"
                .into(),
        ));
    }

    let mut text_edits: Vec<(Uri, Vec<TextEdit>)> = Vec::new();
    for file in files {
        let path = file
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpClientError::UntranslatableEdit(
                format!("an edited file carries no `path`: {file}"),
            ))?;
        let abs = absolutize(path, workspace_path);
        let uri = path_to_uri(&abs).ok_or_else(|| {
            McpClientError::UntranslatableEdit(format!(
                "cannot form a file:// URI for `{}`",
                abs.display()
            ))
        })?;
        let edits = as_array(&file, "edits")?
            .into_iter()
            .map(text_edit_from_henka)
            .collect::<Result<Vec<_>, _>>()?;
        text_edits.push((uri, edits));
    }

    let mut resource_ops: Vec<ResourceOp> = Vec::new();
    for op in file_ops {
        resource_ops.push(resource_op_from_henka(&op, workspace_path)?);
    }

    if supports_document_changes {
        let mut ops: Vec<DocumentChangeOperation> = text_edits
            .into_iter()
            .map(|(uri, edits)| {
                DocumentChangeOperation::Edit(tower_lsp_server::lsp_types::TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                })
            })
            .collect();
        ops.extend(resource_ops.into_iter().map(DocumentChangeOperation::Op));
        return Ok(WorkspaceEdit {
            changes: None,
            document_changes: (!ops.is_empty()).then_some(DocumentChanges::Operations(ops)),
            change_annotations: None,
        });
    }

    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for (uri, edits) in text_edits {
        changes.entry(uri).or_default().extend(edits);
    }
    Ok(WorkspaceEdit {
        changes: (!changes.is_empty()).then_some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

/// Read an array field, treating a missing field as empty but a present
/// non-array as a payload the proxy doesn't understand.
fn as_array(value: &serde_json::Value, key: &str) -> Result<Vec<serde_json::Value>, McpClientError> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => Ok(items.clone()),
        Some(other) => Err(McpClientError::UntranslatableEdit(format!(
            "`{key}` is not an array: {other}"
        ))),
    }
}

fn text_edit_from_henka(edit: serde_json::Value) -> Result<TextEdit, McpClientError> {
    let malformed = || {
        McpClientError::UntranslatableEdit(format!("cannot decode a text edit: {edit}"))
    };
    let read = |v: &serde_json::Value, key: &str| -> Result<u32, McpClientError> {
        v.get(key)
            .and_then(|n| n.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(malformed)
    };
    let range = edit.get("range").ok_or_else(malformed)?;
    let start = range.get("start").ok_or_else(malformed)?;
    let end = range.get("end").ok_or_else(malformed)?;
    let range = Range {
        start: Position {
            line: read(start, "line")?,
            character: read(start, "character")?,
        },
        end: Position {
            line: read(end, "line")?,
            character: read(end, "character")?,
        },
    };
    let new_text = edit
        .get("new_text")
        .and_then(|v| v.as_str())
        .ok_or_else(malformed)?
        .to_string();
    Ok(TextEdit { range, new_text })
}

fn resource_op_from_henka(
    op: &serde_json::Value,
    workspace_path: &Path,
) -> Result<ResourceOp, McpClientError> {
    let malformed = |detail: &str| {
        McpClientError::UntranslatableEdit(format!("cannot decode a file operation: {detail}: {op}"))
    };
    let uri_field = |key: &str| -> Result<Uri, McpClientError> {
        let path = op
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| malformed(&format!("no `{key}`")))?;
        path_to_uri(&absolutize(path, workspace_path))
            .ok_or_else(|| malformed(&format!("`{key}` is not a usable path")))
    };
    match op.get("op").and_then(|v| v.as_str()) {
        Some("create") => Ok(ResourceOp::Create(CreateFile {
            uri: uri_field("path")?,
            options: None,
            annotation_id: None,
        })),
        Some("delete") => Ok(ResourceOp::Delete(DeleteFile {
            uri: uri_field("path")?,
            options: None,
        })),
        Some("rename") => Ok(ResourceOp::Rename(RenameFile {
            old_uri: uri_field("from")?,
            new_uri: uri_field("to")?,
            options: None,
            annotation_id: None,
        })),
        Some(other) => Err(McpClientError::UntranslatableEdit(format!(
            "unknown file operation `{other}`; the proxy would silently drop it"
        ))),
        None => Err(malformed("no `op`")),
    }
}

fn absolutize(path: &str, workspace_path: &Path) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace_path.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    #[test]
    fn uri_to_path_strips_prefix_and_decodes() {
        let uri = Uri::from_str("file:///root/stargate/src/Foo%20Bar.java").unwrap();
        assert_eq!(
            uri_to_path(&uri).unwrap(),
            PathBuf::from("/root/stargate/src/Foo Bar.java")
        );
    }

    #[test]
    fn path_to_uri_encodes_spaces() {
        let uri = path_to_uri(Path::new("/tmp/a b.txt")).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/a%20b.txt");
    }

    #[test]
    fn uri_roundtrip() {
        let p = PathBuf::from("/root/stargate/src/Foo.java");
        let uri = path_to_uri(&p).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn usages_response_maps_to_locations() {
        let response = json!({
            "count": 2,
            "usages": [
                { "file": "src/Foo.java", "start_line": 3, "start_character": 4,
                  "end_line": 3, "end_character": 7 },
                { "file": "src/Bar.java", "start_line": 10, "start_character": 0,
                  "end_line": 10, "end_character": 3 },
            ],
        });
        let root = Path::new("/root/stargate");
        let locs = usages_to_locations(response, root).unwrap();
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[0].uri.as_str(), "file:///root/stargate/src/Foo.java");
        assert_eq!(locs[0].range.start.line, 3);
        assert_eq!(locs[0].range.end.character, 7);
    }

    /// A one-file, one-edit payload in the shape henka's dry-run response
    /// carries under `edit`.
    fn simple_edit() -> serde_json::Value {
        json!({
            "encoding": "Utf16",
            "files": [
                {
                    "path": "src/Foo.java",
                    "edits": [
                        {
                            "range": {
                                "start": { "line": 1, "character": 4 },
                                "end": { "line": 1, "character": 7 }
                            },
                            "new_text": "Bar"
                        }
                    ]
                }
            ],
            "file_ops": []
        })
    }

    #[test]
    fn a_document_changes_client_gets_only_document_changes() {
        let ws_edit =
            henka_edit_to_workspace_edit(&simple_edit(), Path::new("/root/stargate"), true).unwrap();
        let Some(DocumentChanges::Operations(ops)) = &ws_edit.document_changes else {
            panic!("expected Operations, got {ws_edit:?}");
        };
        assert_eq!(ops.len(), 1);
        // Emitting `changes` as well would be applied twice by a client that
        // reads both maps.
        assert!(
            ws_edit.changes.is_none(),
            "documentChanges and changes must not both be populated"
        );
    }

    #[test]
    fn a_client_without_document_changes_gets_the_changes_map() {
        let ws_edit = henka_edit_to_workspace_edit(&simple_edit(), Path::new("/root/stargate"), false)
            .unwrap();
        let changes = ws_edit.changes.as_ref().expect("changes map populated");
        let uri = Uri::from_str("file:///root/stargate/src/Foo.java").unwrap();
        assert_eq!(changes.get(&uri).unwrap().len(), 1);
        assert!(ws_edit.document_changes.is_none());
    }

    #[test]
    fn henka_edit_carries_file_ops() {
        let edit = json!({
            "encoding": "Utf16",
            "files": [],
            "file_ops": [
                { "op": "rename", "from": "src/Foo.java", "to": "src/Bar.java" }
            ]
        });
        let ws_edit =
            henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate"), true).unwrap();
        let Some(DocumentChanges::Operations(ops)) = &ws_edit.document_changes else {
            panic!("expected Operations, got {ws_edit:?}");
        };
        assert!(matches!(&ops[0], DocumentChangeOperation::Op(ResourceOp::Rename(_))));
    }

    #[test]
    fn file_ops_need_a_client_that_can_express_them() {
        // The `changes` map has no way to say "rename this file", and dropping
        // the op would apply the text edits to files that should have moved.
        let edit = json!({
            "encoding": "Utf16",
            "files": [],
            "file_ops": [{ "op": "rename", "from": "src/Foo.java", "to": "src/Bar.java" }]
        });
        let err = henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate"), false)
            .expect_err("must not silently drop the rename");
        assert!(err.to_string().contains("documentChanges"), "got: {err}");
    }

    #[test]
    fn non_utf16_encoding_is_refused() {
        // Taking a UTF-8 offset as UTF-16 misplaces every edit on a line with
        // non-ASCII text, in the user's files.
        let mut edit = simple_edit();
        edit["encoding"] = json!("Utf8");
        let err = henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate"), true)
            .expect_err("must not apply an edit in another encoding");
        assert!(err.to_string().contains("Utf8"), "got: {err}");
    }

    #[test]
    fn a_malformed_edit_fails_the_whole_request() {
        // One undecodable edit among several: applying the rest would leave the
        // tree half-renamed with nothing to say so.
        let mut edit = simple_edit();
        edit["files"].as_array_mut().unwrap().push(json!({
            "path": "src/Bar.java",
            "edits": [{ "range": { "start": { "line": 2 } }, "new_text": "Bar" }]
        }));
        let err = henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate"), true)
            .expect_err("a partial refactor must not be handed to the client");
        assert!(err.to_string().contains("text edit"), "got: {err}");
    }

    #[test]
    fn a_file_without_a_path_fails_the_request() {
        let mut edit = simple_edit();
        edit["files"].as_array_mut().unwrap()[0]
            .as_object_mut()
            .unwrap()
            .remove("path");
        let err = henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate"), true)
            .expect_err("an edit to an unnamed file must not be dropped");
        assert!(err.to_string().contains("`path`"), "got: {err}");
    }

    #[test]
    fn an_unknown_file_operation_fails_the_request() {
        let edit = json!({
            "encoding": "Utf16",
            "files": [],
            "file_ops": [{ "op": "teleport", "path": "src/Foo.java" }]
        });
        let err = henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate"), true)
            .expect_err("an operation the proxy cannot express must not be dropped");
        assert!(err.to_string().contains("teleport"), "got: {err}");
    }

    #[test]
    fn an_edit_with_no_encoding_field_is_taken_as_utf16() {
        // Older henka-server payloads omit it; UTF-16 is what they meant.
        let mut edit = simple_edit();
        edit.as_object_mut().unwrap().remove("encoding");
        assert!(henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate"), true).is_ok());
    }

    #[test]
    fn null_or_missing_usages_becomes_empty() {
        assert!(usages_to_locations(json!(null), Path::new("/x")).unwrap().is_empty());
        assert!(
            usages_to_locations(json!({ "count": 0 }), Path::new("/x"))
                .unwrap()
                .is_empty()
        );
    }
}
