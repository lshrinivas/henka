//! Conversions between LSP wire types and henka's JSON tool-call shape.
//!
//! Kept in one module so the position-encoding contract (UTF-16 both sides)
//! and URI ↔ path handling live in one place.

use std::path::{Path, PathBuf};

use tower_lsp_server::lsp_types::{Location, Position, Range, Uri};

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
    fn null_or_missing_usages_becomes_empty() {
        assert!(usages_to_locations(json!(null), Path::new("/x")).unwrap().is_empty());
        assert!(
            usages_to_locations(json!({ "count": 0 }), Path::new("/x"))
                .unwrap()
                .is_empty()
        );
    }
}
