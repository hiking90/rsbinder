// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Why a serve loop ended — the value [`RpcSession::serve_blocking`] and
//! its siblings return.
//!
//! A serve loop can end for a dozen reasons, and the questions a caller
//! asks about them are few: *is the stream still intact where the loop
//! left it?* (decides whether anything more can be read), *did this end
//! decide to stop?* (decides whether the end was expected), and *what
//! happened?* (goes in the log). Before this type those three answers
//! were spread over a `Result<bool>`, a `StatusCode` and a per-slot
//! flag, and whatever none of them could carry ended up in prose. Here
//! each is an axis: [`StreamState`], [`EndedBy`], [`EndReason`].
//!
//! [`SessionEnd::into_result`] is the one projection back to
//! `Result<()>`, and it reads a single axis — see its table.
//!
//! [`RpcSession::serve_blocking`]: super::RpcSession::serve_blocking

use crate::StatusCode;

/// Who ended the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndedBy {
    /// This end decided to: an explicit
    /// [`RpcSession::shutdown`](super::RpcSession::shutdown) or
    /// [`RpcServer::terminate`](super::RpcServer::terminate), or a
    /// deadline this end armed and let expire between frames (idle
    /// eviction). Every end observed after that decision is `Local`,
    /// whichever side's bytes reached the loop first.
    Local,
    /// Not this end's decision: the peer closed, went away, or the
    /// stream failed. Whether the *peer* chose it is not knowable at the
    /// transport — a close, a reset and a cut all arrive as an end of
    /// stream — so this says only what it can.
    NotLocal,
}

/// Whether the stream is still known to be intact where the loop left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamState {
    /// The read stopped at a frame boundary with nothing unaccounted
    /// for: an end of stream that was signaled, a deadline that elapsed
    /// between frames, or a slot this end had already retired.
    InSync,
    /// Not known to be intact — a frame stopped part-way or did not
    /// decode, a nested call lost the position, or the stream ended
    /// without its close signal. Nothing further should be read from it,
    /// and the peer's side of the story is not known either.
    Lost,
}

/// What ended the loop — the mechanism, for the log and for telling
/// apart ends the two axes above fold together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndReason {
    /// The read reached end of stream at a frame boundary.
    EndOfStream,
    /// The stream ended without the transport's close signal
    /// ([`RpcError::UncleanEndOfStream`](super::RpcError::UncleanEndOfStream)).
    UncleanEndOfStream,
    /// A nested call made from a handler on this connection lost track
    /// of what the peer sends next and marked the slot unreadable.
    Unreadable,
    /// The slot was gone from the pool when the loop went to pin it —
    /// retired by a transaction that failed on it, or by the session
    /// ending.
    Retired,
    /// This end ended the session while the loop held a frame it had
    /// just read; the frame was not dispatched.
    Interrupted,
    /// A deadline this end armed elapsed part-way through a frame
    /// ([`RpcError::DeadlineMidFrame`](super::RpcError::DeadlineMidFrame)):
    /// this end's own decision, and a lost position.
    DeadlineMidFrame,
    /// Reading or decoding a frame failed; the code is what the read or
    /// the decoder produced (`TimedOut` for a deadline that elapsed
    /// between frames, `NotEnoughData` for one that cut a frame, …).
    Frame(StatusCode),
    /// Dispatching a frame failed — the handler, or writing its reply.
    Dispatch(StatusCode),
}

/// Why a serve loop ended. Returned by
/// [`RpcSession::serve_blocking`](super::RpcSession::serve_blocking),
/// [`serve_blocking_on`](super::RpcSession::serve_blocking_on) and
/// [`serve_blocking_clearing_deadline_after_first`](super::RpcSession::serve_blocking_clearing_deadline_after_first).
/// Whatever the reason, every remote object reachable over the session
/// is dead once the loop has ended and its death recipients have fired;
/// this value says how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SessionEnd {
    /// Who ended it.
    pub by: EndedBy,
    /// Whether the stream is still known to be intact.
    pub stream: StreamState,
    /// What happened.
    pub reason: EndReason,
}

impl SessionEnd {
    /// Build the value from the mechanism and whether this end had
    /// already decided to end the session. The two axes follow from
    /// those; the rules are the whole contract, so they are listed:
    ///
    /// | reason | `by` | `stream` |
    /// |---|---|---|
    /// | `EndOfStream` | local decision? | `InSync` |
    /// | `UncleanEndOfStream`, `Unreadable`, `Frame(_)` (a frame cut or undecodable) | local decision? | `Lost` |
    /// | `Frame(TimedOut)` (a deadline between frames) | `Local` | `InSync` |
    /// | `DeadlineMidFrame` (a deadline inside one) | `Local` | `Lost` |
    /// | `Interrupted` | `Local` | `InSync` |
    /// | `Retired`, `Dispatch(_)` | local decision? | `InSync` if this end decided, else `Lost` |
    ///
    /// "Local decision?" is `Local` when this end had decided, else
    /// `NotLocal`. A fault this loop observed itself (a cut, an
    /// undecodable frame, a lost position) is `Lost` whoever decided;
    /// only the ambiguous ends — a slot already gone, a dispatch that
    /// failed — are read in the light of who ended the session.
    pub(crate) fn new(reason: EndReason, ended_locally: bool) -> Self {
        use EndReason::*;
        use StreamState::*;
        let decided = ended_locally
            || matches!(
                reason,
                Interrupted | DeadlineMidFrame | Frame(StatusCode::TimedOut)
            );
        let by = if decided {
            EndedBy::Local
        } else {
            EndedBy::NotLocal
        };
        let stream = match reason {
            EndOfStream | Interrupted | Frame(StatusCode::TimedOut) => InSync,
            UncleanEndOfStream | Unreadable | DeadlineMidFrame | Frame(_) => Lost,
            Retired | Dispatch(_) => {
                if ended_locally {
                    InSync
                } else {
                    Lost
                }
            }
        };
        SessionEnd { by, stream, reason }
    }

    /// The one projection to `Result<()>`, for a caller that only wants
    /// "did it end well". It reads a single axis:
    ///
    /// | `stream` | result |
    /// |---|---|
    /// | `InSync` | `Ok(())` — the end was clean, whoever ended it |
    /// | `Lost` | `Err(`[`StatusCode::DeadObject`]`)` — the stream is not to be trusted, and the session is over |
    ///
    /// Who ended it does not change the result — a session this end shut
    /// down and one whose peer closed both end cleanly — and the
    /// specific cause is in [`reason`](Self::reason), not in the code:
    /// every lost stream is one `DeadObject`, because that is the state
    /// the session is in, and a log line wants the reason, not the code.
    pub fn into_result(self) -> crate::Result<()> {
        match self.stream {
            StreamState::InSync => Ok(()),
            StreamState::Lost => Err(StatusCode::DeadObject),
        }
    }

    /// `true` when the stream was intact at the end —
    /// [`into_result`](Self::into_result) would be `Ok(())`.
    pub fn is_clean(&self) -> bool {
        matches!(self.stream, StreamState::InSync)
    }

    /// Log this end at the level it deserves — a lost stream is worth a
    /// `warn!`, every clean end is `debug!` however it came about.
    pub(crate) fn log(&self, what: &str) {
        match self.stream {
            StreamState::Lost => log::warn!("{what}: {self}"),
            StreamState::InSync => log::debug!("{what}: {self}"),
        }
    }
}

impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let by = match self.by {
            EndedBy::Local => "ended by this end",
            EndedBy::NotLocal => "ended by the peer or the stream",
        };
        let stream = match self.stream {
            StreamState::InSync => "stream intact",
            StreamState::Lost => "stream lost",
        };
        write!(f, "{by}, {stream} ({:?})", self.reason)
    }
}

/// One step of the serve loop.
pub(crate) enum ServeStep {
    /// A frame was handled; read the next one.
    Continue,
    /// The loop is over, for this reason.
    Ended(EndReason),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The projection table, cell by cell — every combination
    /// [`SessionEnd::new`] can produce, and what it projects to.
    #[test]
    fn projection_reads_the_stream_axis_only() {
        use EndReason::*;
        use EndedBy::{Local, NotLocal};
        use StatusCode::{BadType, DeadObject, NotEnoughData, TimedOut};
        use StreamState::{InSync, Lost};
        // (reason, this end had decided, expected by, expected stream, clean?)
        let cases: &[(EndReason, bool, EndedBy, StreamState, bool)] = &[
            (EndOfStream, false, NotLocal, InSync, true),
            (EndOfStream, true, Local, InSync, true),
            (UncleanEndOfStream, false, NotLocal, Lost, false),
            (UncleanEndOfStream, true, Local, Lost, false),
            (Unreadable, false, NotLocal, Lost, false),
            (Unreadable, true, Local, Lost, false),
            (Retired, false, NotLocal, Lost, false),
            (Retired, true, Local, InSync, true),
            (Interrupted, true, Local, InSync, true),
            (DeadlineMidFrame, false, Local, Lost, false),
            (Frame(TimedOut), false, Local, InSync, true),
            (Frame(NotEnoughData), false, NotLocal, Lost, false),
            (Frame(BadType), true, Local, Lost, false),
            (Dispatch(DeadObject), false, NotLocal, Lost, false),
            (Dispatch(DeadObject), true, Local, InSync, true),
        ];
        for &(reason, local, by, stream, ok) in cases {
            let end = SessionEnd::new(reason, local);
            assert_eq!(end.by, by, "{reason:?} local={local}");
            assert_eq!(end.stream, stream, "{reason:?} local={local}");
            assert_eq!(end.is_clean(), ok, "{reason:?} local={local}");
            let expected = if ok {
                Ok(())
            } else {
                Err(StatusCode::DeadObject)
            };
            assert_eq!(end.into_result(), expected, "{reason:?} local={local}");
        }
    }
}
