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
/// responses since the `Include structured WorkspaceEdit in dry_run` change)
/// into an LSP `WorkspaceEdit`.
///
/// Uses `documentChanges` because henka may emit `file_ops` (create / rename /
/// delete) alongside text edits, which the `changes` map alone can't express.
/// Text-only edits still land in `documentChanges` as `TextDocumentEdit`s —
/// this way there's one path for both cases.
pub fn henka_edit_to_workspace_edit(
    value: &serde_json::Value,
    workspace_path: &Path,
) -> Result<WorkspaceEdit, McpClientError> {
    let files = value
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let file_ops = value
        .get("file_ops")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    for file in files {
        let Some(path) = file.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let abs = absolutize(path, workspace_path);
        let Some(uri) = path_to_uri(&abs) else {
            continue;
        };
        let edits: Vec<TextEdit> = file
            .get("edits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(text_edit_from_henka)
            .collect();
        ops.push(DocumentChangeOperation::Edit(
            tower_lsp_server::lsp_types::TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits: edits.into_iter().map(OneOf::Left).collect(),
            },
        ));
    }

    for op in file_ops {
        let Some(kind) = op.get("op").and_then(|v| v.as_str()) else {
            continue;
        };
        let resource_op = match kind {
            "create" => op
                .get("path")
                .and_then(|v| v.as_str())
                .and_then(|p| path_to_uri(&absolutize(p, workspace_path)))
                .map(|uri| ResourceOp::Create(CreateFile { uri, options: None, annotation_id: None })),
            "delete" => op
                .get("path")
                .and_then(|v| v.as_str())
                .and_then(|p| path_to_uri(&absolutize(p, workspace_path)))
                .map(|uri| ResourceOp::Delete(DeleteFile { uri, options: None })),
            "rename" => {
                let from = op
                    .get("from")
                    .and_then(|v| v.as_str())
                    .and_then(|p| path_to_uri(&absolutize(p, workspace_path)));
                let to = op
                    .get("to")
                    .and_then(|v| v.as_str())
                    .and_then(|p| path_to_uri(&absolutize(p, workspace_path)));
                match (from, to) {
                    (Some(old_uri), Some(new_uri)) => Some(ResourceOp::Rename(RenameFile {
                        old_uri,
                        new_uri,
                        options: None,
                        annotation_id: None,
                    })),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(op) = resource_op {
            ops.push(DocumentChangeOperation::Op(op));
        }
    }

    let document_changes = if ops.is_empty() {
        None
    } else {
        Some(DocumentChanges::Operations(ops))
    };

    // Some LSP clients only look at `changes`. Fill it in from the text edits
    // for maximum compatibility; the resource ops still need documentChanges.
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    if let Some(DocumentChanges::Operations(ops)) = &document_changes {
        for op in ops {
            if let DocumentChangeOperation::Edit(edit) = op {
                changes
                    .entry(edit.text_document.uri.clone())
                    .or_default()
                    .extend(edit.edits.iter().filter_map(|oneof| match oneof {
                        OneOf::Left(te) => Some(te.clone()),
                        OneOf::Right(_) => None,
                    }));
            }
        }
    }

    Ok(WorkspaceEdit {
        changes: if changes.is_empty() { None } else { Some(changes) },
        document_changes,
        change_annotations: None,
    })
}

fn text_edit_from_henka(edit: serde_json::Value) -> Option<TextEdit> {
    let range = edit.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let range = Range {
        start: Position {
            line: start.get("line")?.as_u64()? as u32,
            character: start.get("character")?.as_u64()? as u32,
        },
        end: Position {
            line: end.get("line")?.as_u64()? as u32,
            character: end.get("character")?.as_u64()? as u32,
        },
    };
    let new_text = edit.get("new_text")?.as_str()?.to_string();
    Some(TextEdit { range, new_text })
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

    #[test]
    fn henka_edit_maps_to_text_document_edits() {
        // Shape: what henka's dry_run response now carries in `edit`.
        let response = json!({
            "edit": {
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
            }
        });
        let root = Path::new("/root/stargate");
        let ws_edit = henka_edit_to_workspace_edit(&response["edit"], root).unwrap();
        // `changes` map populated for legacy clients.
        let changes = ws_edit.changes.as_ref().unwrap();
        let uri = Uri::from_str("file:///root/stargate/src/Foo.java").unwrap();
        assert_eq!(changes.get(&uri).unwrap().len(), 1);
        // documentChanges carries the same edit.
        assert!(matches!(
            ws_edit.document_changes.as_ref().unwrap(),
            DocumentChanges::Operations(_)
        ));
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
        let ws_edit = henka_edit_to_workspace_edit(&edit, Path::new("/root/stargate")).unwrap();
        let Some(DocumentChanges::Operations(ops)) = &ws_edit.document_changes else {
            panic!("expected Operations, got {ws_edit:?}");
        };
        assert!(matches!(&ops[0], DocumentChangeOperation::Op(ResourceOp::Rename(_))));
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
