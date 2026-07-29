//! The published schemas are what the repository holds (US-018 AC3).
//!
//! `pyxis app-server --emit-schemas <dir>` writes the same bytes this test
//! compares, from the same generator. Regenerate with:
//!
//! ```text
//! PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use agent_app_server::schema::{
    JSON_SCHEMA_FILE, TYPESCRIPT_FILE, json_schema_document, typescript,
};

fn published_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/app-server")
        .canonicalize()
        .expect("the published schema directory exists")
}

fn generated() -> [(PathBuf, String); 2] {
    let dir = published_dir();
    let mut document =
        serde_json::to_string_pretty(&json_schema_document()).expect("serializable document");
    document.push('\n');
    [
        (dir.join(JSON_SCHEMA_FILE), document),
        (dir.join(TYPESCRIPT_FILE), typescript()),
    ]
}

#[test]
fn the_published_schemas_match_the_protocol_types() {
    let update = std::env::var_os("PYXIS_UPDATE_SCHEMAS").is_some();
    for (path, expected) in generated() {
        if update {
            std::fs::write(&path, &expected).expect("the published schema is writable");
            continue;
        }
        let found = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            found,
            expected,
            "{} is stale; regenerate with PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas",
            path.display()
        );
    }
}
