//! What a memory server tells its clients' connectors of a scope's
//! memories, so a push on a server's store can name a memory's title and
//! mark an agent's note (#292): each memory's title, source and type,
//! never its text. The connector keeps them in memory
//! only; nothing here is written on the client.

use super::{Position, delivery::Error};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};

/// One memory as a title line shows it: its title, and its source and type,
/// which say how the line marks it (`super::push::mark`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Titled {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) source: String,
    #[serde(rename = "type")]
    pub(crate) memory_type: String,
}

/// A server's answer for one scope, at the store's `position`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Answer {
    /// The store's position when it answered.
    pub(crate) position: String,
    /// Whether `memories` is every memory of the scope in force, rather
    /// than those changed since the position asked with.
    pub(crate) whole: bool,
    /// The memories of the scope in force: all of them when `whole`, else
    /// those changed since, as they are now.
    pub(crate) memories: Vec<Titled>,
    /// The memories changed since that the scope doesn't hold now, by id
    /// alone: forgotten, ended, superseded, moved out of the scope, or of
    /// another project all along. A memory moved to another project is
    /// still in force, so only this list takes it out of a client's
    /// titles of the scope it left.
    pub(crate) removed: Vec<String>,
}

/// The memories a title line may name in a scope: those the delivery
/// index holds ([`super::delivery::delivered`]), so a line comes only for a
/// memory the hooks can match.
const IN_SCOPE: &str = concat!(
    "SELECT m.id,m.title,m.source,m.memory_type FROM memories m WHERE ",
    super::delivery::delivered!("m")
);

/// The answer for the scope `projects` from the store at `db`, read without
/// writing in one transaction: the memories changed since the position
/// `since` when the store still counts them, else every memory of the
/// scope.
pub(crate) fn answer(db: &Path, projects: &[String], since: Option<&str>) -> Result<Answer, Error> {
    let mut conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let conn = conn.transaction()?;
    let (store_id, change, pruned): (String, i64, i64) = conn.query_row(
        "SELECT store_id,last_change,pruned_through FROM store_changes WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let position = Position::at(&store_id, change, 0);
    let scope = super::delivery::json_array(projects);
    let since = since
        .map(|since| Position::kept(since.to_owned()))
        .filter(|since| since.within(&position))
        .and_then(|since| since.change())
        .filter(|since| *since >= pruned);
    let mut answer = Answer {
        position: position.as_str().to_owned(),
        whole: since.is_none(),
        ..Answer::default()
    };
    match since {
        None => answer.memories = titled(&conn, IN_SCOPE, [scope.clone()])?,
        Some(since) => {
            let changed: Vec<String> = super::delivery::changed_since(&conn, since)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            if !changed.is_empty() {
                let query = format!("{IN_SCOPE} AND m.id IN (SELECT value FROM json_each(?2))");
                answer.memories = titled(
                    &conn,
                    &query,
                    [scope.clone(), super::delivery::json_array(&changed)],
                )?;
                let found: HashSet<&str> = answer.memories.iter().map(|m| m.id.as_str()).collect();
                answer.removed = changed
                    .iter()
                    .filter(|id| !found.contains(id.as_str()))
                    .cloned()
                    .collect();
            }
        }
    }
    Ok(answer)
}

/// The memories `query` selects with `params`.
fn titled<const N: usize>(
    conn: &Connection,
    query: &str,
    params: [String; N],
) -> rusqlite::Result<Vec<Titled>> {
    let mut statement = conn.prepare(query)?;
    let rows = statement.query_map(params_from_iter(params), |r| {
        Ok(Titled {
            id: r.get(0)?,
            title: r.get(1)?,
            source: r.get(2)?,
            memory_type: r.get(3)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "github.com/example/app";
    const OTHER: &str = "github.com/example/other";

    /// Records a stated memory titled `title` in `project`, or global for
    /// none, in the store at `db`; its id.
    fn record(db: &Path, title: &str, project: Option<&str>) -> String {
        record_typed(db, "fact", title, project)
    }

    /// [`record`] of type `memory_type`.
    fn record_typed(db: &Path, memory_type: &str, title: &str, project: Option<&str>) -> String {
        let input = serde_json::json!({
            "type": memory_type, "category": "build", "title": title, "content": title,
            "scope": if project.is_some() { "project" } else { "global" },
            "project": project.unwrap_or_default(), "user_confirmed": true,
        });
        let out = crate::memory::record(db, serde_json::from_value(input).unwrap()).unwrap();
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    fn titles(answer: &Answer) -> Vec<&str> {
        let mut titles: Vec<&str> = answer.memories.iter().map(|m| m.title.as_str()).collect();
        titles.sort_unstable();
        titles
    }

    /// Whole, a scope's answer holds its memories and the global ones,
    /// never another project's. Since a position, it holds what changed
    /// since, as it is now, and names what left the scope; a position of
    /// another store, or one it no longer counts from, gets it whole.
    #[test]
    fn an_answer_holds_the_scope_whole_or_its_changes_since() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memories.sqlite3");
        let global = record(&db, "Prefer straight quotes", None);
        let app = record(&db, "The app's door sticks", Some(APP));
        record(&db, "The other's secret name", Some(OTHER));
        // What the user does, which the delivery index never holds either.
        record_typed(&db, "behavior", "Runs the tests twice", None);
        record_typed(&db, "pattern", "Works late on releases", Some(APP));
        let scope = [APP.to_owned()];
        let whole = answer(&db, &scope, None).unwrap();
        assert!(whole.whole);
        assert_eq!(
            titles(&whole),
            ["Prefer straight quotes", "The app's door sticks"]
        );
        assert!(whole.removed.is_empty());

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE memories SET title='Prefer curly quotes' WHERE id=?1",
            [&global],
        )
        .unwrap();
        conn.execute("DELETE FROM memories WHERE id=?1", [&app])
            .unwrap();
        let since = answer(&db, &scope, Some(&whole.position)).unwrap();
        assert!(!since.whole);
        assert_eq!(titles(&since), ["Prefer curly quotes"]);
        assert_eq!(since.removed, [app]);
        assert_ne!(since.position, whole.position);

        let unchanged = answer(&db, &scope, Some(&since.position)).unwrap();
        assert!(!unchanged.whole);
        assert!(unchanged.memories.is_empty() && unchanged.removed.is_empty());

        for foreign in ["1.0123456789abcdef.1.0", "not a position"] {
            let again = answer(&db, &scope, Some(foreign)).unwrap();
            assert!(again.whole, "{foreign}");
            assert_eq!(titles(&again), ["Prefer curly quotes"]);
        }
        conn.execute(
            "UPDATE store_changes SET pruned_through=last_change WHERE id=1",
            [],
        )
        .unwrap();
        assert!(answer(&db, &scope, Some(&whole.position)).unwrap().whole);
    }
}
