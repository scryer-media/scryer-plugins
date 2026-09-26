//! Paging cursors and change fingerprints.
//!
//! The host pages until `next_cursor` is empty and fails a sync that needs
//! more than a hundred pages, so every paged source caps itself well below
//! that. It sends `since_fingerprint` and honours `unchanged` only on the
//! first page, which makes a fingerprint sound only for a source that answers
//! in one response: paged sources leave it unset.

use scryer_plugin_sdk::{ListPluginFetchResponse, ListPluginItem, PluginError};

use crate::error::permanent;

/// Decode a numeric cursor (a page number or an offset). No cursor means the
/// first page, which is `first`.
pub fn numeric_cursor(cursor: Option<&str>, first: u32) -> Result<u32, PluginError> {
    match cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) {
        None => Ok(first),
        Some(cursor) => cursor
            .parse::<u32>()
            .map_err(|_| permanent(format!("invalid page cursor {cursor}"))),
    }
}

/// A stable digest of the list's contents: every item key and rank in order,
/// plus the list name.
pub fn fingerprint(items: &[ListPluginItem], list_name: Option<&str>) -> String {
    // FNV-1a: tiny, dependency-free and stable across builds and targets.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(list_name.unwrap_or_default().as_bytes());
    for item in items {
        feed(b"\n");
        feed(item.item_key.as_bytes());
        feed(b"#");
        feed(item.rank.unwrap_or_default().to_string().as_bytes());
    }
    format!("v1:{hash:016x}:{}", items.len())
}

/// Finish a source that answers in a single response: attach the
/// fingerprint and, when it matches the one the host already holds, return
/// no items and `unchanged`.
pub fn single_page(
    items: Vec<ListPluginItem>,
    list_name: Option<String>,
    list_url: Option<String>,
    since_fingerprint: Option<&str>,
) -> ListPluginFetchResponse {
    let fingerprint = fingerprint(&items, list_name.as_deref());
    if since_fingerprint == Some(fingerprint.as_str()) {
        return ListPluginFetchResponse {
            list_name,
            list_url,
            fingerprint: Some(fingerprint),
            unchanged: true,
            ..ListPluginFetchResponse::default()
        };
    }
    ListPluginFetchResponse {
        total_hint: Some(items.len() as u32),
        items,
        next_cursor: None,
        list_name,
        list_url,
        fingerprint: Some(fingerprint),
        unchanged: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, rank: u32) -> ListPluginItem {
        ListPluginItem {
            item_key: key.to_string(),
            rank: Some(rank),
            ..Default::default()
        }
    }

    #[test]
    fn cursor_defaults_and_rejects_garbage() {
        assert_eq!(numeric_cursor(None, 1).unwrap(), 1);
        assert_eq!(numeric_cursor(Some(""), 0).unwrap(), 0);
        assert_eq!(numeric_cursor(Some("7"), 1).unwrap(), 7);
        assert!(numeric_cursor(Some("seven"), 1).is_err());
    }

    #[test]
    fn fingerprint_tracks_membership_order_and_name() {
        let base = fingerprint(&[item("a", 1), item("b", 2)], Some("L"));
        assert_eq!(base, fingerprint(&[item("a", 1), item("b", 2)], Some("L")));
        assert_ne!(base, fingerprint(&[item("b", 1), item("a", 2)], Some("L")));
        assert_ne!(base, fingerprint(&[item("a", 1)], Some("L")));
        assert_ne!(base, fingerprint(&[item("a", 1), item("b", 2)], Some("M")));
    }

    #[test]
    fn matching_fingerprint_short_circuits() {
        let first = single_page(vec![item("a", 1)], None, None, None);
        assert!(!first.unchanged);
        assert_eq!(first.items.len(), 1);
        let again = single_page(vec![item("a", 1)], None, None, first.fingerprint.as_deref());
        assert!(again.unchanged);
        assert!(again.items.is_empty());
        assert_eq!(again.fingerprint, first.fingerprint);
    }
}
