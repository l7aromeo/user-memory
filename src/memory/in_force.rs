//! The one test of "in force", as an SQL fragment: in a module of its own
//! so the delivery benchmark's mirror of `src/memory` compiles it too.

/// The test of "in force" over a row of `memories` named `$table` (the
/// table, or an alias of it or of `search_keys`, which carry `status` and
/// `id`): confirmed or tentative, and not in `out_of_force`. The status is a
/// column test and `out_of_force` a small set SQLite builds once per
/// statement, so a statement that reads every memory, as search's
/// candidates do, costs about what the status test alone does; a lookup
/// per row would cost about 100 ms more at 100,000 memories. Written out
/// only here: the view `live_memories`, `schema::LIVE` and the delivery
/// queries expand it.
macro_rules! live {
    ($table:literal) => {
        concat!(
            $table,
            ".status IN ('confirmed','tentative') AND ",
            $table,
            ".id NOT IN (SELECT id FROM out_of_force)"
        )
    };
}
pub(crate) use live;
