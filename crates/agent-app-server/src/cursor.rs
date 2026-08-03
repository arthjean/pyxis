//! Opaque pagination cursors (FR-17).
//!
//! A cursor is an index, and an index alone would be portable across threads:
//! a client could take the cursor of one list and apply it to another. It
//! carries its owner instead, and a cursor whose owner is not the thread being
//! listed is refused rather than silently applied.

use crate::jsonrpc::{ErrorObject, error_code};

pub fn encode(thread_id: &str, index: usize) -> String {
    hex::encode(format!("{thread_id}:{index}"))
}

pub fn decode(cursor: &str, thread_id: &str) -> Result<usize, ErrorObject> {
    let invalid = || {
        ErrorObject::new(
            error_code::INVALID_PARAMS,
            "cursor is not a cursor this thread handed out",
        )
    };
    let decoded =
        String::from_utf8(hex::decode(cursor).map_err(|_| invalid())?).map_err(|_| invalid())?;
    let (owner, index) = decoded.rsplit_once(':').ok_or_else(invalid)?;
    if owner != thread_id {
        return Err(invalid());
    }
    index.parse::<usize>().map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_is_opaque_and_bound_to_its_thread() {
        let cursor = encode("thr_1", 50);
        assert!(!cursor.contains("thr_1"), "{cursor}");
        assert!(!cursor.contains("50"), "{cursor}");
        assert_eq!(decode(&cursor, "thr_1"), Ok(50));
        assert!(decode(&cursor, "thr_2").is_err());
        assert!(decode("zz", "thr_1").is_err());
        assert!(decode("abc", "thr_1").is_err());
    }
}
