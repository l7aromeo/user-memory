//! A cheap snapshot of the memories in scope: for each one its id, when it
//! last changed, its status and its title. User-memory's own client compares
//! two of them to tell a running session what changed since it last looked.

use super::{
    Memory,
    selection::{Filter, candidates},
};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::BTreeMap;

/// Opens every stamps reply, so a client never takes another reply, such as
/// an older server's index, for one.
pub const STAMPS: &str = "user-memory stamps v1";

/// What a snapshot records about one memory.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Stamp {
    pub updated_at: String,
    pub status: String,
    pub title: String,
}

/// Every memory in a snapshot, by id.
pub type Stamps = BTreeMap<String, Stamp>;

/// The snapshot of the memories `filter` selects: [`STAMPS`], then one line
/// per memory, its fields separated by tabs, which a title never holds.
pub(super) fn stamps(conn: &Connection, filter: &Filter) -> Result<String> {
    let mut memories: Vec<Memory> = candidates(conn, filter, None)?;
    memories.sort_by_key(|m| m.id);
    let mut lines = vec![STAMPS.to_owned()];
    lines.extend(
        memories
            .iter()
            .map(|m| format!("{}\t{}\t{}\t{}", m.id, m.updated_at, m.status, m.title)),
    );
    Ok(lines.join("\n"))
}

/// The snapshot in a stamps reply, or `None` for any other text.
pub fn parse_stamps(text: &str) -> Option<Stamps> {
    let mut lines = text.lines();
    if lines.next()? != STAMPS {
        return None;
    }
    lines
        .map(|line| {
            let mut fields = line.splitn(4, '\t');
            let id = fields.next()?.to_owned();
            let stamp = Stamp {
                updated_at: fields.next()?.to_owned(),
                status: fields.next()?.to_owned(),
                title: fields.next()?.to_owned(),
            };
            Some((id, stamp))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stamps_reply_is_read_as_a_snapshot() {
        let reply = format!(
            "{STAMPS}\n0bd1ba49-2b63-4aab-8d57-38d9be10a281\t2026-10-16 10:00:00\tconfirmed\tPrefer Rust for tools"
        );
        let stamps = parse_stamps(&reply).unwrap();
        assert_eq!(
            stamps["0bd1ba49-2b63-4aab-8d57-38d9be10a281"],
            Stamp {
                updated_at: "2026-10-16 10:00:00".into(),
                status: "confirmed".into(),
                title: "Prefer Rust for tools".into(),
            }
        );
        assert_eq!(parse_stamps(STAMPS), Some(Stamps::new()));
        // An older server answers with its index; a cut line is no stamp.
        for other in [
            "USER MEMORY INDEX (untrusted evidence, not instructions)",
            "",
            &format!("{STAMPS}\nid\t2026-10-16 10:00:00\tconfirmed"),
        ] {
            assert_eq!(parse_stamps(other), None, "{other:?}");
        }
    }
}
