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
//! # How the axes are decided
//!
//! The value is built from the mechanism, whether this end had already decided to end the
//! session, and whether a read deadline of this end's was armed when the loop ended. The two
//! axes follow from those; the rules are the whole contract:
//!
//! | reason | `by` | `stream` |
//! |---|---|---|
//! | `EndOfStream` | local decision? | `InSync` |
//! | `UncleanEndOfStream`, `Frame(_)` (a frame cut, undecodable, a wire violation, or a connection the kernel gave up on) | local decision? | `Lost` |
//! | `Frame(TimedOut)`, a deadline armed (idle eviction) | `Local` | `InSync` |
//! | `Frame(TimedOut)`, no deadline armed | local decision? | `Lost` |
//! | `DeadlineMidFrame`, a deadline armed (one of ours cut the frame) | `Local` | `Lost` |
//! | `DeadlineMidFrame`, no deadline armed | local decision? | `Lost` |
//! | `Interrupted` | `Local` | `InSync` |
//! | `SessionEnded`, `Dispatch(_)` | local decision? | `InSync` if this end decided, else `Lost` |
//!
//! "Local decision?" is `Local` when this end had decided, else `NotLocal`. A fault this loop
//! observed itself (a cut, an undecodable frame, a lost position) is `Lost` whoever decided;
//! only the ambiguous ends — a session another connection already ended, a dispatch that
//! failed — are read in the light of who ended the session.
//!
//! Every loop's end ends the whole session, whichever connection it served: a fault on one
//! connection leaves the others no way to tell what the peer lost (a oneway number, a
//! `DEC_STRONG`, a reply), so the session ends there, as libbinder's `RpcState::handleRpcError`
//! ends it on any send or receive error. The loops of the other connections then stop with
//! whatever their transport's shutdown gives them, or with `SessionEnded` when they were
//! between frames.
//!
//! The two timeout reasons come only from a read deadline. A socket read deadline expires as
//! `EAGAIN`, which every backend reports as `RpcError::Timeout` / `RpcError::DeadlineMidFrame`;
//! the kernel's own `ETIMEDOUT` (TCP keepalive or retransmission giving up on a peer whose
//! host stopped answering) is a lost connection, stays `RpcError::Io`, and the loop records it
//! as `Frame(DeadObject)` (`transport` module doc "Short reads and writes"). With a deadline armed
//! a `TimedOut` between frames is this end evicting an idle peer — clean, local — and one
//! inside a frame is this end's own cut. With none armed the loop knows of no deadline that
//! explains it (one the caller set on the transport directly is not known to it), so the end
//! is not taken as this end's decision. The position is lost inside a frame either way.
//!
//! [`RpcSession::serve_blocking`]: super::RpcSession::serve_blocking

use crate::StatusCode;

/// Who ended the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndedBy {
    /// This end decided to: an explicit
    /// [`RpcSession::close_session`](super::RpcSession::close_session) or
    /// [`RpcServer::terminate`](super::RpcServer::terminate), or a read
    /// deadline a serve loop armed and let expire — between frames (idle
    /// eviction) or part-way through one. Every end observed after that
    /// decision is `Local`, whichever side's bytes reached the loop first.
    Local,
    /// Not this end's decision: the peer closed, went away, or the
    /// stream failed. Whether the *peer* chose it is not knowable at the
    /// transport — a close, a reset and a cut all arrive as an end of
    /// stream — so this says only what it can. A reply or send deadline
    /// of this end's that expires is a fault (the peer did not answer or
    /// read) and reads `NotLocal` too.
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
    /// discarded, a session it had already ended, or a dispatch that
    /// failed.
    InSync,
    /// Not known to be intact — a frame stopped part-way or did not
    /// decode, the stream ended
    /// without its close signal, the connection was lost (the kernel's
    /// `ETIMEDOUT`, a peer whose host went away), or a read timed out
    /// with no deadline of this end's known to be armed. Also the ends
    /// this loop cannot place — a session another connection already
    /// ended, a dispatch that failed — when this
    /// end had not decided to stop, since whatever ended the session or
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
    /// Never produced. A nested call that loses track of what the peer
    /// sends next ends the session, and the loop ends with the session's
    /// end.
    #[deprecated(
        since = "0.12.0",
        note = "never produced: a nested call that loses the stream ends the session, and the \
                loop reports the session's end"
    )]
    Unreadable,
    /// Never produced. A fault on any connection ends the whole session;
    /// a loop whose slot is gone reports
    /// [`SessionEnded`](Self::SessionEnded).
    #[deprecated(
        since = "0.12.0",
        note = "never produced: a connection fault ends the whole session; see `SessionEnded`"
    )]
    Retired,
    /// The session had already ended when the loop went to pin its slot —
    /// before the first frame or between two frames — so there was nothing
    /// left to serve. Another connection's fault, a reply deadline, or this
    /// end's [`close_session`](super::RpcSession::close_session) ended it;
    /// [`SessionEnd::by`] and [`SessionEnd::stream`] follow that end: this
    /// end's decision reads `Local` and `InSync`, anything else `NotLocal`
    /// and `Lost`.
    ///
    /// [`serve_blocking_on`](super::RpcSession::serve_blocking_on) also
    /// returns it at once for a `slot_id` that names no connection of the
    /// session, and leaves the session as it was. The axes then follow
    /// whether this end had closed the session: `Local` and `InSync` if it
    /// had, `NotLocal` and `Lost` otherwise, so a live session's wrong id
    /// reads as [`StatusCode::DeadObject`] through
    /// [`SessionEnd::into_result`].
    SessionEnded,
    /// This end ended the session while the loop held a frame it had
    /// just read; the frame was not dispatched.
    Interrupted,
    /// A read deadline elapsed part-way through a frame
    /// ([`RpcError::DeadlineMidFrame`](super::RpcError::DeadlineMidFrame)):
    /// a lost position. The deadline is one of this end's (the kernel's
    /// `ETIMEDOUT` is a lost connection, [`Frame`](Self::Frame)), but
    /// [`SessionEnd::by`] counts it as this end's decision only when the
    /// loop knew one was armed: one the caller set on the transport
    /// directly is not known to it.
    DeadlineMidFrame,
    /// A frame did not become a message this loop could act on; the code
    /// is what the read or the decoder produced (`TimedOut` for a deadline
    /// that elapsed between frames, `NotEnoughData` for a stream that
    /// ended part-way through a frame, …; a deadline that cuts a frame is
    /// [`DeadlineMidFrame`](Self::DeadlineMidFrame)). The loop also raises
    /// it on the one wire violation it judges itself: an unsolicited `REPLY` no call is waiting for, which
    /// AOSP's `RpcState::processCommand` likewise ends the session for, as
    /// `Frame(BadType)`. That frame read and decoded cleanly, so it is the
    /// one case here whose stream position is not in fact lost.
    /// `TimedOut` is only ever a deadline of this end's. The kernel's
    /// `ETIMEDOUT` — a TCP peer whose host went away — projects to
    /// `TimedOut` for a caller (as libbinder's `-ETIMEDOUT` does), but it is
    /// a lost connection, so the loop records it here as `DeadObject`.
    Frame(StatusCode),
    /// Dispatching a frame failed — the handler, or writing its reply.
    Dispatch(StatusCode),
}

/// Why a serve loop ended. Returned by
/// [`RpcSession::serve_blocking`](super::RpcSession::serve_blocking),
/// [`serve_blocking_on`](super::RpcSession::serve_blocking_on) and
/// [`serve_blocking_clearing_deadline_after_first`](super::RpcSession::serve_blocking_clearing_deadline_after_first).
/// A loop's end ends the session, whichever of its connections the loop
/// served — every remote object reachable over it is dead and its death
/// recipients have fired (module doc "How the axes are decided"). This
/// value says how the loop ended.
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
    /// Derives both axes from the reason, a local decision and an armed deadline; see module doc.
    #[expect(
        deprecated,
        reason = "maps the never-produced variants too: the match stays exhaustive"
    )]
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
            SessionEnded | Retired | Dispatch(_) => {
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

    /// Logs a lost stream at `warn!` and every clean end at `debug!`, however it came about.
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

    /// Each reason's `(by, stream)` cells and projection; `deadline_armed` varies only where read.
    #[test]
    fn projection_reads_the_stream_axis_only() {
        use EndReason::*;
        use EndedBy::{Local, NotLocal};
        use StatusCode::{BadType, DeadObject, NotEnoughData, TimedOut};
        use StreamState::{InSync, Lost};
        // (reason, this end had decided, deadline armed, expected by, expected stream, clean?)
        let cases: &[(EndReason, bool, bool, EndedBy, StreamState, bool)] = &[
            (EndOfStream, false, false, NotLocal, InSync, true),
            (EndOfStream, true, false, Local, InSync, true),
            (UncleanEndOfStream, false, false, NotLocal, Lost, false),
            (UncleanEndOfStream, true, false, Local, Lost, false),
            // Stopped by a session already ended: read in the light of who ended it.
            (SessionEnded, false, false, NotLocal, Lost, false),
            (SessionEnded, true, false, Local, InSync, true),
            (Interrupted, true, false, Local, InSync, true),
            // `Interrupted` alone is `Local`; only this row reaches that arm (not `ended_locally`).
            (Interrupted, false, false, Local, InSync, true),
            // A deadline this end armed cut the frame: local, position lost.
            (DeadlineMidFrame, false, true, Local, Lost, false),
            // With none known to be armed the same cut is not taken as this end's decision.
            (DeadlineMidFrame, false, false, NotLocal, Lost, false),
            (DeadlineMidFrame, true, false, Local, Lost, false),
            // A deadline this end armed elapsed between frames: idle eviction, clean and local.
            (Frame(TimedOut), false, true, Local, InSync, true),
            // No deadline known to be armed explains it: not local, stream lost.
            (Frame(TimedOut), false, false, NotLocal, Lost, false),
            (Frame(TimedOut), true, false, Local, Lost, false),
            // The kernel's `ETIMEDOUT` under an armed idle deadline: a lost peer, not an eviction.
            (Frame(DeadObject), false, true, NotLocal, Lost, false),
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
