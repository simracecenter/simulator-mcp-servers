// SPDX-License-Identifier: GPL-3.0-or-later
//! Generic "send → poll → verify" loop shared by every `<sim>-mcp` adapter's
//! command tools.
//!
//! Promoted out of `iracing-mcp`'s inline verification loops (originally
//! duplicated across `replay_set_playback`, `replay_seek_session_time`,
//! `replay_seek_frame`, and `camera_focus` in
//! `crates/iracing-mcp/src/handler.rs`) so `lmu-mcp`'s input-buffer command
//! path can reuse the same shape — per
//! [ADR 0002 D2](../../../docs/adr/0002-lmu-adapter-design.md), `LmuAdapter`
//! also needs to verify effect by polling the same shared-memory family it
//! reads from, just without iRacing's OS-broadcast acknowledgement gap.
//!
//! [`verify_loop`] is generic over a state type `S` and error type `E` rather
//! than any `IracingAdapter`/`LmuAdapter` trait, matching ADR 0001 D1's
//! layering: `mcp-core` has no simulator-specific knowledge. Translating the
//! returned [`VerifyOutcome`] into a `tools/call` JSON-RPC response stays in
//! each `<sim>-mcp` crate's own handler.

use std::future::Future;
use std::time::Duration;

use tokio::time::{sleep, Instant};

/// Result of a completed [`verify_loop`] call.
///
/// Both variants carry the same fields (`before`, `observed`, `elapsed`) so
/// callers can shape either into a JSON-RPC response without matching on
/// differently-shaped types — [`VerifyOutcome::is_verified`] is the only
/// thing that tells them apart.
#[derive(Debug, Clone)]
pub enum VerifyOutcome<S> {
    /// The verify predicate returned `true` before the timeout elapsed.
    Verified {
        before: S,
        observed: S,
        elapsed: Duration,
    },
    /// The timeout elapsed without the verify predicate ever returning
    /// `true`.
    TimedOut {
        before: S,
        observed: S,
        elapsed: Duration,
    },
}

impl<S> VerifyOutcome<S> {
    /// Whether this outcome represents a verified (not timed-out) result.
    pub fn is_verified(&self) -> bool {
        matches!(self, VerifyOutcome::Verified { .. })
    }

    /// The state observed immediately before `send`, regardless of variant.
    pub fn before(&self) -> &S {
        match self {
            VerifyOutcome::Verified { before, .. } | VerifyOutcome::TimedOut { before, .. } => {
                before
            }
        }
    }

    /// The last polled state, regardless of variant.
    pub fn observed(&self) -> &S {
        match self {
            VerifyOutcome::Verified { observed, .. } | VerifyOutcome::TimedOut { observed, .. } => {
                observed
            }
        }
    }

    /// Wall-clock time elapsed between `send` completing and the final poll.
    pub fn elapsed(&self) -> Duration {
        match self {
            VerifyOutcome::Verified { elapsed, .. } | VerifyOutcome::TimedOut { elapsed, .. } => {
                *elapsed
            }
        }
    }
}

/// Runs a generic "send a command, then poll until a condition is verified
/// or a timeout elapses" loop.
///
/// - `before`: the state observed immediately before `send`, carried through
///   into the returned [`VerifyOutcome`] for the caller to include in its own
///   response — not otherwise used by this function.
/// - `send`: a one-shot future that issues the command. Awaited exactly once,
///   before the first poll.
/// - `poll`: a repeatable closure returning a future that fetches the latest
///   state. Called at least once, and again every `poll_interval` until
///   `verify` returns `true` or `timeout` elapses.
/// - `verify`: a predicate over the polled state. May hold its own mutable
///   state across calls (e.g. tracking a candidate value seen on a previous
///   poll before treating it as settled) — not just a stateless check.
/// - `timeout` / `poll_interval`: caller-supplied, since call sites vary
///   these per command.
///
/// Returns `Err(E)` if `send` or any `poll` call fails. No error/response
/// shaping happens here — the caller shapes that into its own JSON-RPC
/// response the same way it always has.
pub async fn verify_loop<S, E, SendFut, PollFn, PollFut, VerifyFn>(
    before: S,
    send: SendFut,
    mut poll: PollFn,
    mut verify: VerifyFn,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<VerifyOutcome<S>, E>
where
    SendFut: Future<Output = Result<(), E>>,
    PollFn: FnMut() -> PollFut,
    PollFut: Future<Output = Result<S, E>>,
    VerifyFn: FnMut(&S) -> bool,
{
    send.await?;

    let started_at = Instant::now();

    loop {
        let observed = poll().await?;
        let verified = verify(&observed);
        let elapsed = started_at.elapsed();

        if verified {
            return Ok(VerifyOutcome::Verified {
                before,
                observed,
                elapsed,
            });
        }

        if elapsed >= timeout {
            return Ok(VerifyOutcome::TimedOut {
                before,
                observed,
                elapsed,
            });
        }

        sleep(poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[tokio::test(start_paused = true)]
    async fn verifies_before_timeout() {
        let mut states: VecDeque<i32> = (0..=5).collect();
        let poll = move || {
            let state = states.pop_front().unwrap_or(5);
            async move { Ok::<i32, String>(state) }
        };

        let outcome = verify_loop(
            0,
            async { Ok::<(), String>(()) },
            poll,
            |observed: &i32| *observed >= 3,
            Duration::from_millis(500),
            Duration::from_millis(10),
        )
        .await
        .expect("poll never errors");

        assert!(outcome.is_verified());
        assert_eq!(*outcome.before(), 0);
        assert_eq!(*outcome.observed(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_with_no_verification() {
        let poll = || async { Ok::<i32, String>(0) };

        let outcome = verify_loop(
            0,
            async { Ok::<(), String>(()) },
            poll,
            |_observed: &i32| false,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .await
        .expect("poll never errors");

        assert!(!outcome.is_verified());
        assert_eq!(*outcome.observed(), 0);
        assert!(outcome.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test(start_paused = true)]
    async fn verify_predicate_may_hold_mutable_state_across_polls() {
        // Mirrors `iracing-mcp`'s `pause_candidate_frame` tracking: a poll is
        // only "verified" once the same value has been observed on two
        // consecutive polls.
        let mut states: VecDeque<i32> = VecDeque::from([1, 2, 5, 5, 5]);
        let poll = move || {
            let state = states.pop_front().unwrap_or(5);
            async move { Ok::<i32, String>(state) }
        };

        let mut candidate: Option<i32> = None;
        let verify = move |observed: &i32| match candidate {
            Some(previous) if previous == *observed => true,
            _ => {
                candidate = Some(*observed);
                false
            }
        };

        let outcome = verify_loop(
            0,
            async { Ok::<(), String>(()) },
            poll,
            verify,
            Duration::from_millis(500),
            Duration::from_millis(10),
        )
        .await
        .expect("poll never errors");

        assert!(outcome.is_verified());
        assert_eq!(*outcome.observed(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn send_error_short_circuits_before_polling() {
        let result = verify_loop(
            0,
            async { Err::<(), &'static str>("send failed") },
            || async { Ok::<i32, &'static str>(0) },
            |_observed: &i32| true,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .await;

        assert_eq!(result.unwrap_err(), "send failed");
    }
}
