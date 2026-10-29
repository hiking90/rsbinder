// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The checks of what a subscription receives (plan 15 fleet test §5.2), as plain functions of
//! their inputs so that a hermetic test can feed them the sequences a defect would produce
//! (§5.5).
//!
//! A node runs a [`Subscription`] over every receive and reports the first violation. The
//! controller runs the rest on the events nodes print, with what it alone knows: where a paused
//! publisher stood when a subscriber connected ([`first_seq`]), how it ended a publisher and the
//! last seq that publisher committed ([`check_end`]).
//!
//! A "connection" is one subscription to one publisher. A `Subscriber` has one; a
//! `NamedSubscriber` has one per publisher it follows, each ended by a `Disconnected`.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use rsbinder_topic::Start;

use super::record::{Incarnation, Stamp};

/// How a subscription receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `recv`, `recv_sample`: every record is returned or counted in a `Lagged`, a slot the
    /// publisher took back from a `recv_sample` as `Lagged(1)`.
    Counted,
    /// `recv_latest`: records are skipped without being counted, so only order and an upper
    /// bound on `Lagged` hold.
    Latest,
}

/// What one connection has received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Count {
    pub incarnation: Incarnation,
    pub received: u64,
    pub lagged: u64,
    /// The first record's seq less the records lagged before it.
    pub first_seq: u64,
    pub last_seq: u64,
}

impl Count {
    /// The last seq the connection returned or counted lost: `last_seq`, or past it by a lag
    /// that ended the connection. Meaningful in [`Mode::Counted`] alone.
    pub fn covered(&self) -> u64 {
        self.first_seq + self.received + self.lagged - 1
    }
}

/// The receives of one subscription, checked as they come.
///
/// A receive that fails a check changes nothing, so the next one is checked against the state
/// before it. A connection that ends after a `Lagged` but before any record loses that lag:
/// no count is made of it.
pub struct Subscription {
    mode: Mode,
    /// The highest seq returned from each publisher some connection read.
    joined: HashMap<Incarnation, u64>,
    count: Option<Count>,
    /// Records lagged since the last record of the connection, or since it started.
    pending: u64,
    /// Records accepted and lagged over every connection, for `SubscriberStats`.
    received: u64,
    lagged: u64,
}

impl Subscription {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            joined: HashMap::new(),
            count: None,
            pending: 0,
            received: 0,
            lagged: 0,
        }
    }

    /// `(received, lagged)` over every connection: what a `NamedSubscriber`'s `SubscriberStats`
    /// counts as `received` and `lost` when no check failed.
    pub fn totals(&self) -> (u64, u64) {
        (self.received, self.lagged)
    }

    /// `(received, lagged)` of the current connection: what a `Subscriber`'s counts, which a
    /// subscription made again starts from zero.
    pub fn current(&self) -> (u64, u64) {
        let (received, lagged) = self
            .count
            .map_or((0, 0), |count| (count.received, count.lagged));
        (received, lagged + self.pending)
    }

    /// Check a record; `Ok(true)` when it is the first of its connection.
    pub fn record(&mut self, stamp: Stamp) -> Result<bool, String> {
        let seq = stamp.seq;
        let pending = self.pending;
        let first = match &self.count {
            None => {
                let first_seq = seq
                    .checked_sub(pending)
                    .ok_or_else(|| format!("{pending} lagged before seq {seq}"))?;
                if let Some(&seen) = self.joined.get(&stamp.incarnation) {
                    if seq <= seen {
                        return Err(format!(
                            "seq {seq} of {} again: an earlier connection returned up to {seen}",
                            stamp.incarnation
                        ));
                    }
                }
                self.count = Some(Count {
                    incarnation: stamp.incarnation,
                    received: 0,
                    lagged: 0,
                    first_seq,
                    last_seq: seq,
                });
                true
            }
            Some(count) => {
                let last = count.last_seq;
                if stamp.incarnation != count.incarnation {
                    return Err(format!(
                        "seq {seq} of {} within a connection to {}",
                        stamp.incarnation, count.incarnation
                    ));
                }
                if seq <= last {
                    return Err(format!("seq {seq} after {last}"));
                }
                let gap = seq - last - 1;
                match self.mode {
                    Mode::Counted if gap > pending => {
                        return Err(format!(
                            "seq {seq} after {last}: {gap} missing, {pending} counted lagged"
                        ))
                    }
                    Mode::Counted | Mode::Latest if gap < pending => {
                        return Err(format!(
                            "Lagged({pending}) over {gap} missing, seq {seq} after {last}"
                        ))
                    }
                    Mode::Counted | Mode::Latest => {}
                }
                false
            }
        };
        if let Some(count) = &mut self.count {
            count.received += 1;
            count.lagged += pending;
            count.last_seq = seq;
        }
        self.pending = 0;
        self.received += 1;
        let seen = self.joined.entry(stamp.incarnation).or_insert(seq);
        *seen = (*seen).max(seq);
        Ok(first)
    }

    pub fn lagged(&mut self, records: u64) -> Result<(), String> {
        if records == 0 {
            return Err("Lagged(0)".into());
        }
        self.pending += records;
        self.lagged += records;
        Ok(())
    }

    /// What the connection so far received, once it read a record.
    pub fn count(&self) -> Option<Count> {
        self.count
    }

    /// End the connection: its count, with the lag after its last record, and the next
    /// receive starts another.
    pub fn end(&mut self) -> Option<Count> {
        let pending = std::mem::take(&mut self.pending);
        let mut count = self.count.take()?;
        count.lagged += pending;
        Some(count)
    }
}

/// Where a publisher stood when a connection to it was made: the seq it publishes next, and
/// the oldest seq its ring keeps. The controller knows it of a publisher it paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Committed {
    pub next: u64,
    pub oldest: u64,
}

/// Which connection of a subscription a connection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum History {
    /// Its first.
    First,
    /// A later one, to a publisher no earlier connection was made to.
    NewPublisher,
    /// A later one, to a publisher an earlier connection was made to.
    Rejoined,
}

/// The seq the first record of a connection has, by `NamedSubscriber`'s rule of where a
/// connection starts, to a publisher that stood `at` and published nothing until then.
pub fn first_seq(start: Start, history: History, at: Committed) -> u64 {
    let start = match history {
        History::First => start,
        History::NewPublisher if start == Start::Last => Start::Last,
        History::NewPublisher => Start::Oldest,
        History::Rejoined => Start::Latest,
    };
    match start {
        Start::Latest => at.next,
        // On a topic with no record yet, as `Latest`.
        Start::Last => at.next.saturating_sub(1),
        Start::Oldest if at.next == 0 => 0,
        Start::Oldest => at.oldest,
    }
}

/// A `CONNECTED`'s first seq against [`first_seq`].
pub fn check_first(expected: u64, first_seq: u64) -> Result<(), String> {
    if first_seq != expected {
        return Err(format!("connected at seq {first_seq}, not {expected}"));
    }
    Ok(())
}

/// How the controller ended the publisher of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Injected {
    /// `SIGKILL`.
    Kill,
    /// `close`, or the drop of the publisher at `exit`.
    Close,
    /// Another process took the name.
    Replace,
    /// Another process took the name, then the replaced one was killed (X12). A drain that had
    /// begun ends `Replaced`, as it ends at a death; a receive that saw the death before the
    /// notice of the name ends the connection as the death's.
    ReplaceThenKill,
}

/// The `ENDED` reason a subscriber reports for `injected`; [`Injected::ReplaceThenKill`] has two.
pub fn end_reasons(injected: Injected, named: bool) -> &'static [&'static str] {
    match (injected, named) {
        (Injected::Kill, false) => &["PublisherDied"],
        (Injected::Kill, true) => &["Ended.PublisherDied"],
        (Injected::Close, false) => &["End"],
        (Injected::Close, true) => &["Ended.End"],
        (Injected::Replace, _) => &["Replaced"],
        (Injected::ReplaceThenKill, _) => &["Replaced", "Ended.PublisherDied"],
    }
}

/// The one `ENDED` reason of `injected`.
///
/// # Panics
///
/// For [`Injected::ReplaceThenKill`], which has two.
pub fn end_reason(injected: Injected, named: bool) -> &'static str {
    match end_reasons(injected, named) {
        [reason] => reason,
        reasons => panic!("{injected:?} ends with any of {reasons:?}"),
    }
}

/// A connection's end against what the controller did and, when it knows it, the last seq the
/// publisher committed.
///
/// Only a drain that ended `Replaced` may count its lost records by a lower bound; any other
/// end returns or counts every record the publisher committed. No end counts past it.
pub fn check_end(
    mode: Mode,
    named: bool,
    injected: Injected,
    reason: &str,
    count: Option<&Count>,
    last_published: Option<u64>,
) -> Result<(), String> {
    let expected = end_reasons(injected, named);
    if !expected.contains(&reason) {
        return Err(format!(
            "ended with {reason}, not {}",
            expected.join(" or ")
        ));
    }
    let (Some(count), Some(published)) = (count, last_published) else {
        return Ok(());
    };
    let reached = match mode {
        Mode::Counted => count.covered(),
        Mode::Latest => count.last_seq,
    };
    if reached > published {
        return Err(format!(
            "reached seq {reached}, past the last published {published}"
        ));
    }
    if mode == Mode::Counted && reason != "Replaced" && reached < published {
        return Err(format!(
            "{reason} after seq {reached}: {} of the published records neither returned nor counted",
            published - reached
        ));
    }
    Ok(())
}

/// The reports a fan-in relay republishes (T5): each reporter's run reports in rising order,
/// with gaps where the relay's subscriber lagged.
#[derive(Default)]
pub struct Reports {
    last: HashMap<(String, Incarnation), u64>,
}

impl Reports {
    pub fn report(
        &mut self,
        reporter: &str,
        incarnation: Incarnation,
        seq: u64,
    ) -> Result<(), String> {
        match self.last.entry((reporter.into(), incarnation)) {
            Entry::Occupied(last) if seq <= *last.get() => Err(format!(
                "report {seq} of {reporter} {incarnation} after {}",
                last.get()
            )),
            Entry::Occupied(mut last) => {
                last.insert(seq);
                Ok(())
            }
            Entry::Vacant(last) => {
                last.insert(seq);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Incarnation = Incarnation { pid: 10, start: 1 };
    const B: Incarnation = Incarnation { pid: 11, start: 2 };

    fn stamp(incarnation: Incarnation, seq: u64) -> Stamp {
        Stamp {
            topic: 1,
            incarnation,
            seq,
        }
    }

    #[derive(Clone)]
    enum Rx {
        Rec(Incarnation, u64),
        Lag(u64),
        End,
    }
    use Rx::*;

    /// Feed `receives`; the index and message of the first violation.
    fn feed(mode: Mode, receives: &[Rx]) -> Result<Vec<Count>, (usize, String)> {
        let mut subscription = Subscription::new(mode);
        let mut ended = Vec::new();
        for (at, receive) in receives.iter().enumerate() {
            match *receive {
                Rec(incarnation, seq) => subscription
                    .record(stamp(incarnation, seq))
                    .map(drop)
                    .map_err(|text| (at, text))?,
                Lag(records) => subscription.lagged(records).map_err(|text| (at, text))?,
                End => ended.extend(subscription.end()),
            }
        }
        ended.extend(subscription.end());
        Ok(ended)
    }

    fn violation_at(mode: Mode, receives: &[Rx]) -> usize {
        match feed(mode, receives) {
            Ok(counts) => panic!("accepted: {counts:?}"),
            Err((at, _)) => at,
        }
    }

    #[test]
    fn counted_receives_that_hold_pass() {
        let counts = feed(
            Mode::Counted,
            &[
                Lag(2),
                Rec(A, 5),
                Rec(A, 6),
                Lag(3),
                Rec(A, 10),
                Lag(4),
                End,
                Rec(B, 0),
                Rec(B, 1),
            ],
        )
        .unwrap();
        let first = Count {
            incarnation: A,
            received: 3,
            lagged: 9,
            first_seq: 3,
            last_seq: 10,
        };
        assert_eq!(counts[0], first);
        assert_eq!(first.covered(), 14);
        assert_eq!(
            counts[1].received + counts[1].lagged,
            counts[1].last_seq - counts[1].first_seq + 1
        );
    }

    #[test]
    fn a_gap_without_lagged_is_refused() {
        assert_eq!(
            violation_at(Mode::Counted, &[Rec(A, 0), Rec(A, 1), Rec(A, 3)]),
            2
        );
        assert_eq!(
            violation_at(Mode::Counted, &[Rec(A, 0), Lag(1), Rec(A, 3)]),
            2
        );
    }

    #[test]
    fn a_lagged_larger_than_its_gap_is_refused() {
        assert_eq!(
            violation_at(Mode::Counted, &[Rec(A, 0), Lag(3), Rec(A, 3)]),
            2
        );
        assert_eq!(
            violation_at(Mode::Latest, &[Rec(A, 0), Lag(3), Rec(A, 3)]),
            2
        );
        assert_eq!(violation_at(Mode::Counted, &[Lag(6), Rec(A, 5)]), 1);
        assert_eq!(violation_at(Mode::Counted, &[Rec(A, 0), Lag(0)]), 1);
    }

    #[test]
    fn a_repeated_or_earlier_seq_is_refused() {
        assert_eq!(violation_at(Mode::Counted, &[Rec(A, 4), Rec(A, 4)]), 1);
        assert_eq!(violation_at(Mode::Latest, &[Rec(A, 4), Rec(A, 3)]), 1);
    }

    #[test]
    fn another_publisher_within_a_connection_is_refused() {
        assert_eq!(violation_at(Mode::Counted, &[Rec(A, 0), Rec(B, 1)]), 1);
    }

    #[test]
    fn latest_skips_records_without_counting_them() {
        feed(Mode::Latest, &[Rec(A, 0), Rec(A, 7), Lag(2), Rec(A, 20)]).unwrap();
    }

    #[test]
    fn a_rejoined_publisher_returns_no_seq_again() {
        let earlier = [Rec(A, 0), Rec(A, 1), Rec(A, 2), End, Rec(B, 0), End];
        let again = [&earlier[..], &[Rec(A, 2)]].concat();
        assert_eq!(violation_at(Mode::Counted, &again), 6);
        feed(Mode::Counted, &[&earlier[..], &[Rec(A, 3)]].concat()).unwrap();
    }

    #[test]
    fn the_start_of_a_restarted_paused_publisher() {
        // X1: a producer that published K records before it registered, then paused.
        let at = Committed { next: 5, oldest: 0 };
        assert_eq!(first_seq(Start::Latest, History::NewPublisher, at), 0);
        assert_eq!(first_seq(Start::Oldest, History::NewPublisher, at), 0);
        assert_eq!(first_seq(Start::Last, History::NewPublisher, at), 4);
        assert_eq!(first_seq(Start::Latest, History::First, at), 5);
        assert_eq!(
            first_seq(
                Start::Last,
                History::First,
                Committed { next: 0, oldest: 0 }
            ),
            0
        );
        assert_eq!(
            first_seq(
                Start::Oldest,
                History::First,
                Committed {
                    next: 900,
                    oldest: 644
                }
            ),
            644
        );
        assert_eq!(first_seq(Start::Oldest, History::Rejoined, at), 5);
        assert!(check_first(0, 0).is_ok());
        assert!(check_first(0, 5).is_err());
        assert!(check_first(4, 0).is_err());
    }

    fn count(first_seq: u64, received: u64, lagged: u64) -> Count {
        Count {
            incarnation: A,
            received,
            lagged,
            first_seq,
            last_seq: first_seq + received - 1,
        }
    }

    #[test]
    fn only_a_replaced_drain_may_count_short() {
        let short = count(0, 90, 5);
        for injected in [Injected::Kill, Injected::Close] {
            let reason = end_reason(injected, true);
            assert!(check_end(
                Mode::Counted,
                true,
                injected,
                reason,
                Some(&short),
                Some(99)
            )
            .is_err());
            assert!(check_end(
                Mode::Counted,
                true,
                injected,
                reason,
                Some(&short),
                Some(94)
            )
            .is_ok());
            assert!(check_end(Mode::Counted, true, injected, reason, Some(&short), None).is_ok());
        }
        let replaced = end_reason(Injected::Replace, true);
        assert!(check_end(
            Mode::Counted,
            true,
            Injected::Replace,
            replaced,
            Some(&short),
            Some(99)
        )
        .is_ok());
        assert!(check_end(
            Mode::Counted,
            true,
            Injected::Replace,
            replaced,
            Some(&short),
            Some(93)
        )
        .is_err());
        assert!(check_end(
            Mode::Latest,
            false,
            Injected::Close,
            "End",
            Some(&short),
            Some(99)
        )
        .is_ok());
    }

    #[test]
    fn an_end_other_than_the_injected_one_is_refused() {
        assert!(check_end(Mode::Counted, false, Injected::Kill, "End", None, None).is_err());
        assert!(check_end(
            Mode::Counted,
            true,
            Injected::Kill,
            "PublisherDied",
            None,
            None
        )
        .is_err());
        assert!(check_end(Mode::Counted, true, Injected::Close, "Replaced", None, None).is_err());
        assert!(check_end(
            Mode::Counted,
            false,
            Injected::Kill,
            "PublisherDied",
            None,
            None
        )
        .is_ok());
    }

    /// X12: either end holds, and only `Replaced` may count short.
    #[test]
    fn a_replaced_then_killed_publisher_ends_either_way() {
        let x12 = Injected::ReplaceThenKill;
        let short = count(0, 90, 5);
        assert!(check_end(Mode::Counted, true, x12, "Replaced", Some(&short), Some(99)).is_ok());
        let died = "Ended.PublisherDied";
        assert!(check_end(Mode::Counted, true, x12, died, Some(&short), Some(94)).is_ok());
        assert!(check_end(Mode::Counted, true, x12, died, Some(&short), Some(99)).is_err());
        assert!(check_end(Mode::Counted, true, x12, "Ended.End", None, None).is_err());
    }

    #[test]
    #[should_panic(expected = "ends with any of")]
    fn end_reason_refuses_an_injection_of_two_reasons() {
        end_reason(Injected::ReplaceThenKill, true);
    }

    #[test]
    fn reports_rise_per_reporter_run() {
        let mut reports = Reports::default();
        reports.report("p1", A, 0).unwrap();
        reports.report("p1", A, 3).unwrap();
        reports.report("p2", A, 0).unwrap();
        reports.report("p1", B, 0).unwrap();
        assert!(reports.report("p1", A, 3).is_err());
        assert!(reports.report("p1", A, 2).is_err());
    }
}
