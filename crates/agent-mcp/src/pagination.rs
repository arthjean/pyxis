//! Bounded following of a paginated listing.
//!
//! `rmcp`'s own `list_all_*` helpers follow cursors with no ceiling, which is a
//! hostile server's easiest allocation primitive above the frame bound. Every
//! axis the server controls is capped here: pages, items, cursor size and cursor
//! repetition (the shape Codex bounds in `codex-rs/codex-mcp/src/pagination.rs`).

use rmcp::model::PaginatedRequestParams;

/// Max pages followed for one listing.
const MAX_LIST_PAGES: usize = 100;
/// Max items collected across every page of one listing.
const MAX_LIST_ITEMS: usize = 1_024;
/// Max size of an opaque cursor echoed back to the server.
const MAX_CURSOR_BYTES: usize = 64 * 1024;

/// Follows a paginated listing to its end, or to the first bound it crosses.
/// `what` names the listed thing in the failure message.
pub(crate) async fn collect_paginated<T, F, Fut>(what: &str, mut fetch: F) -> Result<Vec<T>, String>
where
    F: FnMut(Option<PaginatedRequestParams>) -> Fut,
    Fut: std::future::Future<Output = Result<(Vec<T>, Option<String>), String>>,
{
    let mut collected: Vec<T> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..MAX_LIST_PAGES {
        let params = cursor
            .take()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let (items, next_cursor) = fetch(params).await?;
        if collected.len() + items.len() > MAX_LIST_ITEMS {
            return Err(format!("more than {MAX_LIST_ITEMS} {what} listed"));
        }
        collected.extend(items);
        let Some(next) = next_cursor else {
            return Ok(collected);
        };
        if next.len() > MAX_CURSOR_BYTES {
            return Err(format!(
                "pagination cursor larger than {MAX_CURSOR_BYTES} bytes"
            ));
        }
        if !seen.insert(next.clone()) {
            return Err("repeated pagination cursor".to_string());
        }
        cursor = Some(next);
    }
    Err(format!("pagination exceeded {MAX_LIST_PAGES} pages"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server that keeps handing out a fresh cursor is stopped by the page cap
    /// rather than followed forever.
    #[tokio::test]
    async fn an_endless_paginator_stops_at_the_page_cap() {
        let page = AtomicUsize::new(0);
        let out: Result<Vec<u8>, String> = collect_paginated("tools", |_| async {
            let n = page.fetch_add(1, Ordering::SeqCst);
            Ok((vec![0_u8], Some(format!("cursor-{n}"))))
        })
        .await;
        assert_eq!(
            out.unwrap_err(),
            format!("pagination exceeded {MAX_LIST_PAGES} pages")
        );
    }

    #[tokio::test]
    async fn a_repeated_cursor_is_refused() {
        let out: Result<Vec<u8>, String> = collect_paginated("tools", |_| async {
            Ok((Vec::new(), Some("same".to_string())))
        })
        .await;
        assert_eq!(out.unwrap_err(), "repeated pagination cursor");
    }

    #[tokio::test]
    async fn an_oversized_cursor_is_refused() {
        let out: Result<Vec<u8>, String> = collect_paginated("tools", |_| async {
            Ok((Vec::new(), Some("c".repeat(MAX_CURSOR_BYTES + 1))))
        })
        .await;
        assert!(out.unwrap_err().contains("cursor larger than"));
    }

    /// The item cap is checked BEFORE the page is appended, so the oversized page
    /// is never allocated into the accumulator.
    #[tokio::test]
    async fn the_item_cap_refuses_before_collecting() {
        let out: Result<Vec<u8>, String> = collect_paginated("resources", |_| async {
            Ok((vec![0_u8; MAX_LIST_ITEMS + 1], None))
        })
        .await;
        assert!(out.unwrap_err().contains("more than 1024 resources"));
    }

    #[tokio::test]
    async fn a_finished_listing_returns_every_page() {
        let page = AtomicUsize::new(0);
        let out: Result<Vec<usize>, String> = collect_paginated("tools", |_| async {
            let n = page.fetch_add(1, Ordering::SeqCst);
            Ok((vec![n], (n < 2).then(|| format!("cursor-{n}"))))
        })
        .await;
        assert_eq!(out.unwrap(), vec![0, 1, 2]);
    }
}
