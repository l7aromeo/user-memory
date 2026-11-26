//! One deadline for all the waits of a tool call (#227). Each wait inside a
//! call is bounded on its own: a write's turns and SQLite's lock
//! ([`super::BUSY`]), an embedding request, a record's draft embedding, the
//! delivery index's lock and a hand-over's migration. A call meeting several
//! in turn would wait for their sum. Within [`until`], every one of them
//! waits at most what is left before the call's deadline ([`wait`]).
//! Outside a call (the CLI, the hooks) no deadline is set and each wait
//! keeps its own bound.

use std::{
    cell::Cell,
    time::{Duration, Instant},
};

/// The longest a tool call's waits may take in all, counted from when the
/// server received the call. A semantic search could otherwise wait about
/// 214 s (two embedding requests of 2 s connect and 60 s each, then a
/// write's three 30 s stages) and a record about 96 s. 90 s covers a cold
/// model's first embedding (60 s) and one write stage after it, and ends
/// before Claude Code moves a call past two minutes to a background task
/// (code.claude.com/docs/en/mcp, "Automatic backgrounding of long tool
/// calls"). Past it the call is answered the way its part answers when it
/// runs out on its own.
const TOOL_CALL_BUDGET: Duration = Duration::from_secs(90);

/// [`TOOL_CALL_BUDGET`], which debug builds take in milliseconds from
/// `USER_MEMORY_TOOL_CALL_BUDGET_MS` so tests can see the budget run out
/// without waiting minutes.
pub(crate) fn tool_call_budget() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("USER_MEMORY_TOOL_CALL_BUDGET_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    TOOL_CALL_BUDGET
}

thread_local! {
    /// The deadline of the tool call this thread is running, if any.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The deadline of the call this thread runs, to hand to a thread it starts
/// for part of the call, which then runs that part inside [`until`].
pub(crate) fn deadline() -> Option<Instant> {
    DEADLINE.get()
}

/// Runs `work` with `deadline` as this thread's deadline, then puts back the
/// one before, even when `work` panics.
pub(crate) fn until<T>(deadline: Option<Instant>, work: impl FnOnce() -> T) -> T {
    /// Puts the earlier deadline back when dropped.
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.set(self.0);
        }
    }
    let _restore = Restore(DEADLINE.replace(deadline));
    work()
}

/// `work` for each of `items`, each on its own scoped thread that keeps
/// this thread's deadline, in order; `None` for one whose thread panicked.
pub(crate) fn scoped<I: Sync, T: Send>(
    items: &[I],
    work: impl Fn(&I) -> T + Sync,
) -> Vec<Option<T>> {
    let deadline = deadline();
    let work = &work;
    std::thread::scope(|threads| {
        let runs: Vec<_> = items
            .iter()
            .map(|item| threads.spawn(move || until(deadline, || work(item))))
            .collect();
        runs.into_iter().map(|run| run.join().ok()).collect()
    })
}

/// How long a wait bounded by `bound` may take now: `bound`, or what is
/// left before this thread's deadline when that is less (zero once it has
/// passed).
pub(crate) fn wait(bound: Duration) -> Duration {
    match DEADLINE.get() {
        Some(deadline) => bound.min(deadline.saturating_duration_since(Instant::now())),
        None => bound,
    }
}

/// What a wait's refusal adds when the call's budget ended the wait rather
/// than its own bound: `, the rest of this tool call's 90 s`, where the
/// wait's own time alone would read as though the holder let go early.
/// Nothing outside a call or before its deadline.
pub(crate) fn ran_out() -> String {
    match DEADLINE.get() {
        Some(deadline) if Instant::now() >= deadline => format!(
            ", the rest of this tool call's {} s",
            tool_call_budget().as_secs()
        ),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bound well past any deadline these tests set.
    const LONG: Duration = Duration::from_secs(3600);

    #[test]
    fn a_wait_outside_a_call_keeps_its_bound() {
        assert_eq!(deadline(), None);
        assert_eq!(wait(LONG), LONG);
    }

    #[test]
    fn a_wait_in_a_call_takes_what_is_left_and_nothing_once_it_has_passed() {
        let left = Duration::from_secs(60);
        until(Some(Instant::now() + left), || {
            let took = wait(LONG);
            assert!(
                took <= left && took > left - Duration::from_secs(5),
                "{took:?}"
            );
            assert_eq!(wait(Duration::from_secs(1)), Duration::from_secs(1));
        });
        until(Some(Instant::now()), || {
            assert_eq!(wait(LONG), Duration::ZERO)
        });
    }

    #[test]
    fn a_refusal_names_the_budget_only_once_it_has_run_out() {
        assert_eq!(ran_out(), "");
        until(Some(Instant::now() + LONG), || assert_eq!(ran_out(), ""));
        until(Some(Instant::now()), || {
            assert_eq!(
                ran_out(),
                format!(
                    ", the rest of this tool call's {} s",
                    tool_call_budget().as_secs()
                )
            );
        });
    }

    #[test]
    fn a_calls_threads_keep_its_deadline() {
        let at = Instant::now() + LONG;
        let seen = until(Some(at), || scoped(&[1, 2, 3], |_| deadline()));
        assert_eq!(seen, [Some(Some(at)); 3]);
        assert_eq!(scoped(&[1], |_| deadline()), [Some(None)]);
    }

    #[test]
    fn the_deadline_is_put_back_after_a_call_and_after_a_panic_in_it() {
        let outer = Instant::now() + LONG;
        until(Some(outer), || {
            until(Some(Instant::now()), || ());
            assert_eq!(deadline(), Some(outer));
            let caught = std::panic::catch_unwind(|| {
                until(Some(Instant::now()), || panic!("a call that panics"));
            });
            assert!(caught.is_err());
            assert_eq!(deadline(), Some(outer));
        });
        assert_eq!(deadline(), None);
    }
}
