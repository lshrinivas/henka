//! Tracking the text of the buffers the client has open.
//!
//! Henka resolves a line/character coordinate against the file as it exists on
//! disk. An LSP client sends coordinates from its buffer, which may hold
//! unsaved edits — and an inserted line above the cursor shifts every position
//! below it, so the operation would silently act on a different symbol. The
//! proxy therefore takes the `textDocumentSync` traffic it would otherwise
//! ignore and uses it for one thing: refusing a request whose buffer no longer
//! matches the file Henka will read.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use tower_lsp_server::lsp_types::Uri;

/// The open buffers, keyed by URI. Full-text sync only (the proxy advertises
/// `TextDocumentSyncKind::FULL`), so each update replaces the whole text.
#[derive(Debug, Default)]
pub struct Documents {
    open: Mutex<HashMap<String, String>>,
}

impl Documents {
    /// Record (or replace) the text of an open buffer.
    pub fn set(&self, uri: &Uri, text: String) {
        if let Ok(mut open) = self.open.lock() {
            open.insert(uri.as_str().to_string(), text);
        }
    }

    /// Forget a buffer the client closed. Its content is whatever is on disk
    /// from here on, which is exactly what Henka reads.
    pub fn remove(&self, uri: &Uri) {
        if let Ok(mut open) = self.open.lock() {
            open.remove(uri.as_str());
        }
    }

    /// Check that a tracked buffer still matches `path` on disk, returning the
    /// reason it doesn't.
    ///
    /// Both unknowns resolve to "no objection": an untracked buffer means the
    /// client never told us about the file, and an unreadable file means the
    /// proxy can't see what Henka sees (a container that doesn't share the
    /// mount) and shouldn't invent an error out of that.
    pub fn check_saved(&self, uri: &Uri, path: &Path) -> Result<(), String> {
        let Ok(open) = self.open.lock() else {
            return Ok(());
        };
        let Some(buffer) = open.get(uri.as_str()) else {
            return Ok(());
        };
        let Ok(on_disk) = std::fs::read_to_string(path) else {
            return Ok(());
        };
        if buffer == &on_disk {
            return Ok(());
        }
        Err(format!(
            "`{}` has unsaved changes. Henka resolves coordinates against the \
             file on disk, so the request would act on a different position \
             than the one in the buffer. Save the file and retry.",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn uri_for(path: &Path) -> Uri {
        Uri::from_str(&format!("file://{}", path.display())).unwrap()
    }

    #[test]
    fn untracked_buffer_raises_no_objection() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Foo.java");
        std::fs::write(&file, "class Foo {}\n").unwrap();

        let docs = Documents::default();
        assert!(docs.check_saved(&uri_for(&file), &file).is_ok());
    }

    #[test]
    fn buffer_matching_disk_passes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Foo.java");
        std::fs::write(&file, "class Foo {}\n").unwrap();

        let docs = Documents::default();
        docs.set(&uri_for(&file), "class Foo {}\n".into());
        assert!(docs.check_saved(&uri_for(&file), &file).is_ok());
    }

    #[test]
    fn dirty_buffer_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Foo.java");
        std::fs::write(&file, "class Foo {}\n").unwrap();

        let docs = Documents::default();
        docs.set(&uri_for(&file), "// added a line\nclass Foo {}\n".into());
        let err = docs
            .check_saved(&uri_for(&file), &file)
            .expect_err("dirty buffer must be reported");
        assert!(err.contains("unsaved changes"), "got: {err}");
    }

    #[test]
    fn closing_a_buffer_stops_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Foo.java");
        std::fs::write(&file, "class Foo {}\n").unwrap();

        let docs = Documents::default();
        docs.set(&uri_for(&file), "dirty\n".into());
        docs.remove(&uri_for(&file));
        assert!(docs.check_saved(&uri_for(&file), &file).is_ok());
    }

    #[test]
    fn missing_file_raises_no_objection() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Gone.java");

        let docs = Documents::default();
        docs.set(&uri_for(&file), "class Gone {}\n".into());
        assert!(docs.check_saved(&uri_for(&file), &file).is_ok());
    }
}
