// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Why a serve loop ended — the value [`RpcSession::serve_blocking`] and
//! its siblings return.
//!
//! A serve loop can end for a dozen reasons, and the questions a caller
//! asks about them are few: *is the stream still intact where the loop
//! left it?* (decides whether anything more can be read), *did this end
//! decide to stop?* (decides whether the end was expected), and *what
//! happened?* (goes in the log). Each is one axis of this value, and no
//! axis is derivable from another: [`StreamState`], [`EndedBy`],
//! [`EndReason`].
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
    /// [`RpcSession::close_session`](super::RpcSession::close_session) or
    /// [`RpcServer::terminate`](super::RpcServer::terminate), or a
    /// deadline this end armed and let expire — between frames (idle
    /// eviction) or part-way through one. Every end observed after that
    /// decision is `Local`, whichever side's bytes reached the loop first.
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
    /// for: an end of stream that was signaled, a deadline *of this
    /// end's* that elapsed between frames, or an end this loop reached
    /// after this end had already decided to stop — a frame it
    /// discarded, a slot it had already retired, or a dispatch that
    /// failed.
    InSync,
    /// Not known to be intact — a frame stopped part-way or did not
    /// decode, a nested call lost the position, the stream ended
    /// without its close signal, or a read timed out with none of this
    /// end's deadlines armed (the kernel's `ETIMEDOUT`, a peer whose
    /// host went away). Also the ends this loop cannot place — a slot
    /// already gone from the pool, a dispatch that failed — when this
    /// end had not decided to stop, since whatever retired the slot or
    /// failed the dispatch may have left a frame half-written. Nothing
    /// further should be read from it, and the peer's side of the story
    /// is not known either.
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
    /// A read deadline elapsed part-way through a frame
    /// ([`RpcError::DeadlineMidFrame`](super::RpcError::DeadlineMidFrame)):
    /// a lost position. Not by itself this end's decision — with none of
    /// this end's deadlines armed it is the kernel's `ETIMEDOUT`, a peer
    /// whose host went away, which is why [`SessionEnd::by`] is decided
    /// with that knowledge.
    DeadlineMidFrame,
    /// A frame did not become a message this loop could act on; the code
    /// is what the read or the decoder produced (`TimedOut` for a deadline
    /// that elapsed between frames, `NotEnoughData` for one that cut a
    /// frame, …). The loop also raises it on the one wire violation it
    /// judges itself: an unsolicited `REPLY` no call is waiting for, which
    /// AOSP's `RpcState::processCommand` likewise ends the session for, as
    /// `Frame(BadType)`. That frame read and decoded cleanly, so it is the
    /// one case here whose stream position is not in fact lost.
    /// `TimedOut` is not by itself this end's deadline: with none armed
    /// it is the kernel's `ETIMEDOUT` — a TCP peer whose host went away
    /// — which is why the axes below are decided with that knowledge.
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
#[must_use = "the serve loop's end says whether the stream was lost; call into_result() or is_clean()"]
pub struct SessionEnd {
    /// Who ended it.
    pub by: EndedBy,
    /// Whether the stream is still known to be intact.
    pub stream: StreamState,
    /// What happened.
    pub reason: EndReason,
}

impl SessionEnd {
    /// Build the value from the mechanism, whether this end had already
    /// decided to end the session, and whether a read deadline of this
    /// end's was armed when the loop ended. The two axes follow from
    /// those; the rules are the whole contract, so they are listed:
    ///
    /// | reason | `by` | `stream` |
    /// |---|---|---|
    /// | `EndOfStream` | local decision? | `InSync` |
    /// | `UncleanEndOfStream`, `Unreadable`, `Frame(_)` (a frame cut, undecodable, or a wire violation) | local decision? | `Lost` |
    /// | `Frame(TimedOut)`, a deadline armed (idle eviction) | `Local` | `InSync` |
    /// | `Frame(TimedOut)`, no deadline armed (the kernel's `ETIMEDOUT`) | local decision? | `Lost` |
    /// | `DeadlineMidFrame`, a deadline armed (one of ours cut the frame) | `Local` | `Lost` |
    /// | `DeadlineMidFrame`, no deadline armed (the kernel's `ETIMEDOUT`) | local decision? | `Lost` |
    /// | `Interrupted` | `Local` | `InSync` |
    /// | `Retired`, `Dispatch(_)` | local decision? | `InSync` if this end decided, else `Lost` |
    ///
    /// "Local decision?" is `Local` when this end had decided, else
    /// `NotLocal`. A fault this loop observed itself (a cut, an
    /// undecodable frame, a lost position) is `Lost` whoever decided;
    /// only the ambiguous ends — a slot already gone, a dispatch that
    /// failed — are read in the light of who ended the session.
    ///
    /// The two timeout reasons need `deadline_armed` because by the time
    /// they reach here the two kinds of timeout have become one. A socket
    /// read deadline arrives as `EAGAIN` and the kernel's `ETIMEDOUT` as
    /// itself, but every backend folds both into `RpcError::Timeout` /
    /// `RpcError::DeadlineMidFrame` (`transport::is_timeout`, which has to
    /// stay that wide: the `Read` adapters carry this end's own deadline
    /// with the `TimedOut` kind). So with a deadline armed a `TimedOut`
    /// between frames reads as this end evicting an idle peer — clean,
    /// local — and one inside a frame as this end's own cut; including a
    /// kernel `ETIMEDOUT` that happens to arrive while one is armed. With
    /// none armed either can only be the kernel's, a TCP/TLS peer whose
    /// host stopped answering, which is not this end's decision. The
    /// position is lost inside a frame whichever it was.
    pub(crate) fn new(reason: EndReason, ended_locally: bool, deadline_armed: bool) -> Self {
        use EndReason::*;
        use StreamState::*;
        let idle_eviction = deadline_armed && matches!(reason, Frame(StatusCode::TimedOut));
        let our_cut = deadline_armed && matches!(reason, DeadlineMidFrame);
        let decided = ended_locally || idle_eviction || our_cut || matches!(reason, Interrupted);
        let by = if decided {
            EndedBy::Local
        } else {
            EndedBy::NotLocal
        };
        let stream = match reason {
            EndOfStream | Interrupted => InSync,
            Frame(StatusCode::TimedOut) if idle_eviction => InSync,
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

    /// The projection table, cell by cell — every `(by, stream)` pair
    /// [`SessionEnd::new`] can produce for each reason, and what it
    /// projects to. `deadline_armed` is varied only where `new` reads it.
    #[test]
    fn projection_reads_the_stream_axis_only() {
        use EndReason::*;
        use EndedBy::{Local, NotLocal};
        use StatusCode::{BadType, DeadObject, NotEnoughData, TimedOut};
        use StreamState::{InSync, Lost};
        // (reason, this end had decided, a deadline was armed,
        //  expected by, expected stream, clean?)
        let cases: &[(EndReason, bool, bool, EndedBy, StreamState, bool)] = &[
            (EndOfStream, false, false, NotLocal, InSync, true),
            (EndOfStream, true, false, Local, InSync, true),
            (UncleanEndOfStream, false, false, NotLocal, Lost, false),
            (UncleanEndOfStream, true, false, Local, Lost, false),
            (Unreadable, false, false, NotLocal, Lost, false),
            (Unreadable, true, false, Local, Lost, false),
            (Retired, false, false, NotLocal, Lost, false),
            (Retired, true, false, Local, InSync, true),
            (Interrupted, true, false, Local, InSync, true),
            // `Interrupted` is `Local` on its own: this end interrupted the
            // loop whether or not it had already decided elsewhere. Only
            // this row holds `new`'s `Interrupted` arm — the one above is
            // already decided by `ended_locally`.
            (Interrupted, false, false, Local, InSync, true),
            // A deadline this end armed cut the frame: local, position lost.
            (DeadlineMidFrame, false, true, Local, Lost, false),
            // With none armed the same cut is the kernel's `ETIMEDOUT`,
            // which this end did not decide.
            (DeadlineMidFrame, false, false, NotLocal, Lost, false),
            (DeadlineMidFrame, true, false, Local, Lost, false),
            // A deadline this end armed elapsed between frames: idle
            // eviction, clean and local.
            (Frame(TimedOut), false, true, Local, InSync, true),
            // The same code with no deadline armed is the kernel's
            // `ETIMEDOUT` — the peer host went away; not this end's
            // decision, and not a stream anything more can be read from.
            (Frame(TimedOut), false, false, NotLocal, Lost, false),
            (Frame(TimedOut), true, false, Local, Lost, false),
            (Frame(NotEnoughData), false, false, NotLocal, Lost, false),
            (Frame(BadType), true, false, Local, Lost, false),
            (Dispatch(DeadObject), false, false, NotLocal, Lost, false),
            (Dispatch(DeadObject), true, false, Local, InSync, true),
        ];
        for &(reason, local, armed, by, stream, ok) in cases {
            let end = SessionEnd::new(reason, local, armed);
            assert_eq!(end.by, by, "{reason:?} local={local} armed={armed}");
            assert_eq!(end.stream, stream, "{reason:?} local={local} armed={armed}");
            assert_eq!(end.is_clean(), ok, "{reason:?} local={local} armed={armed}");
            let expected = if ok {
                Ok(())
            } else {
                Err(StatusCode::DeadObject)
            };
            assert_eq!(
                end.into_result(),
                expected,
                "{reason:?} local={local} armed={armed}"
            );
        }
    }
}
