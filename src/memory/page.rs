//! Cursor pagination for results that could grow with the store (the index,
//! a category read, search), so no single result is unbounded.
//!
//! A cursor names a position in one result's order and carries a fingerprint
//! of that whole order. A cursor from before a store change no longer
//! matches it, and is refused with a request to start again, so a page never
//! silently skips or repeats a memory.

use anyhow::{Result, bail};
use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Memories per page unless asked otherwise: a realistic store fits one page.
pub(super) const PAGE_SIZE: usize = 100;
/// Largest page: about the most full memories one tool result should carry.
const MAX_PAGE_SIZE: usize = 200;
/// Tags the cursor format, so a cursor from another format is refused.
const CURSOR_TAG: &str = "p1";

#[derive(Args, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct Paging {
    /// The next_cursor of the previous page, to continue that result.
    #[arg(long)]
    #[serde(default)]
    pub cursor: Option<String>,
    /// Memories per page: 100 unless set, at most 200.
    #[arg(long)]
    #[serde(default)]
    pub page_size: Option<usize>,
}

/// FNV-1a over the ids in order: a fingerprint of the whole result.
fn fingerprint(ids: &[Uuid]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in ids.iter().flat_map(|id| id.as_bytes().iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Ends the refusal of a cursor, so a caller can tell it from other errors.
const START_AGAIN: &str = "start again without a cursor";

fn start_again() -> anyhow::Error {
    anyhow::anyhow!(
        "This cursor does not belong to the current result (the memories changed since, or it was mistyped); {START_AGAIN}"
    )
}

/// Whether `message` is a cursor's refusal: the result changed since its page.
pub fn stale_cursor(message: &str) -> bool {
    message.contains(START_AGAIN)
}

/// The page of `items` (whose ids are `ids`, in order) that `paging` asks
/// for, and the cursor of the next page when more remain.
pub(super) fn page<T>(
    items: Vec<T>,
    ids: &[Uuid],
    paging: &Paging,
) -> Result<(Vec<T>, Option<String>)> {
    let size = paging.page_size.unwrap_or(PAGE_SIZE);
    if size == 0 || size > MAX_PAGE_SIZE {
        bail!("page_size must be between 1 and {MAX_PAGE_SIZE}");
    }
    let whole = fingerprint(ids);
    let start = match paging.cursor.as_deref().map(str::trim) {
        None | Some("") => 0,
        Some(cursor) => {
            let mut parts = cursor.split('.');
            let (Some(CURSOR_TAG), Some(offset), Some(hash), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(start_again());
            };
            let offset: usize = offset.parse().map_err(|_| start_again())?;
            let hash = u64::from_str_radix(hash, 16).map_err(|_| start_again())?;
            if hash != whole || offset == 0 || offset >= items.len() {
                return Err(start_again());
            }
            offset
        }
    };
    let end = start.saturating_add(size).min(items.len());
    let next = (end < items.len()).then(|| format!("{CURSOR_TAG}.{end}.{whole:016x}"));
    Ok((
        items.into_iter().skip(start).take(end - start).collect(),
        next,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: u128) -> Vec<Uuid> {
        (1..=n).map(Uuid::from_u128).collect()
    }

    fn paging(cursor: Option<&str>, size: Option<usize>) -> Paging {
        Paging {
            cursor: cursor.map(str::to_owned),
            page_size: size,
        }
    }

    #[test]
    fn pages_cover_every_item_once_and_the_last_has_no_cursor() {
        let all = ids(250);
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        let mut sizes = Vec::new();
        loop {
            let (items, next) = page(all.clone(), &all, &paging(cursor.as_deref(), None)).unwrap();
            sizes.push(items.len());
            seen.extend(items);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(sizes, [100, 100, 50]);
        assert_eq!(seen, all);
        let (items, next) = page(all.clone(), &all, &paging(None, Some(200))).unwrap();
        assert_eq!((items.len(), next.is_some()), (200, true));
        let (items, next) = page(ids(3), &ids(3), &paging(None, None)).unwrap();
        assert_eq!((items.len(), next), (3, None));
        let (items, next) = page(Vec::<Uuid>::new(), &[], &paging(None, None)).unwrap();
        assert_eq!((items.len(), next), (0, None));
    }

    #[test]
    fn the_fingerprint_is_fnv_1a_over_the_ids_in_order() {
        // 16 zero bytes, the nil id.
        assert_eq!(fingerprint(&[Uuid::nil()]), 0x8820_1fb9_60ff_6465);
        assert_ne!(
            fingerprint(&ids(2)),
            fingerprint(&[Uuid::from_u128(2), Uuid::from_u128(1)])
        );
    }

    #[test]
    fn a_cursor_with_the_right_fingerprint_must_still_point_inside_the_result() {
        let all = ids(150);
        let whole = fingerprint(&all);
        for offset in [0, 150, 999] {
            let cursor = format!("{CURSOR_TAG}.{offset}.{whole:016x}");
            assert!(
                page(all.clone(), &all, &paging(Some(&cursor), None)).is_err(),
                "{offset}"
            );
        }
        let cursor = format!("{CURSOR_TAG}.149.{whole:016x}");
        let (items, next) = page(all.clone(), &all, &paging(Some(&cursor), None)).unwrap();
        assert_eq!((items, next), (vec![Uuid::from_u128(150)], None));
    }

    #[test]
    fn only_a_cursor_refusal_reads_as_stale() {
        let all = ids(150);
        let changed = ids(151);
        let (_, next) = page(all.clone(), &all, &paging(None, None)).unwrap();
        let err = page(changed.clone(), &changed, &paging(next.as_deref(), None)).unwrap_err();
        assert!(stale_cursor(&err.to_string()));
        assert!(!stale_cursor("database is locked"));
    }

    #[test]
    fn a_cursor_from_another_result_or_mistyped_is_refused() {
        let all = ids(150);
        let (_, next) = page(all.clone(), &all, &paging(None, None)).unwrap();
        let next = next.unwrap();
        let changed = ids(151);
        let err = page(changed.clone(), &changed, &paging(Some(&next), None)).unwrap_err();
        assert!(err.to_string().contains("start again"), "{err}");
        for bad in [
            "x",
            "p1.100",
            "p1.abc.00",
            "p2.100.00",
            "p1.0.00",
            &format!("{next}.1"),
        ] {
            assert!(
                page(all.clone(), &all, &paging(Some(bad), None)).is_err(),
                "{bad}"
            );
        }
        assert!(page(all.clone(), &all, &paging(None, Some(0))).is_err());
        let err = page(all.clone(), &all, &paging(None, Some(201))).unwrap_err();
        assert_eq!(err.to_string(), "page_size must be between 1 and 200");
    }
}
