// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A reader's position and the record it owes, kept together so that every move of the position
//! keeps them consistent.
//!
//! The losses a reader reports add up to the records it did not return only while it knows where
//! the first record it has neither returned nor counted lies: at the position, or before it after
//! a lap. [`Owed`] says which, and each method here keeps it so. The position moves only over a
//! record whose header a judgment found whole ([`Judged`]), or, after a lap, to a record start a
//! counter of the publisher's gives; a header is never believed for a move before its judgment.
//! A drain (`Lap::Drain`) refuses a lap ([`jump`](Cursor::jump)) to its end or past it, so a lap
//! never makes it count a record there; a step over a judged record may reach the end, and
//! [`step`](Cursor::step) says when it does, as [`count_to_end`](Cursor::count_to_end) moves to
//! it.
//!
//! A lapped drain resumes at `tail` ([`Resume::Tail`]) until two laps in a row find it still on
//! the record a resume at `tail` put it on: the copy or the judgment of that record was overtaken
//! before the drain moved past it, twice, so the publisher writes over the oldest record faster
//! than the drain reaches it. One such lap does not turn it: a resume that counts a loss returns
//! the `Lagged` first and stays on the oldest record, which one commit before the next receive
//! writes over. A lap anywhere else starts the count again. From the second lap on the drain
//! resumes at `half` ([`Resume::Half`]), as a live reader does; [`lapped`](Cursor::lapped)
//! decides it, at a lap only, and a new drain ([`stop_at`](Cursor::stop_at)) starts at `tail`
//! again. A publisher that has stopped writing overtakes nothing at or after the `tail` it left,
//! so it never turns a drain to `half`.

use super::rules::Header;
use super::Judged;

/// The first record a reader has neither returned nor counted lost, as far as it knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owed {
    /// No record read yet: nothing before the first one read is counted.
    Unanchored,
    /// Record `next` starts at the position; every record before it was returned or counted.
    Here { next: u32 },
    /// Record `next` lies at or before the position; `credited` from it on were counted already.
    Owing { next: u32, credited: u32 },
}

/// Where a lapped reader resumes, and where it stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lap {
    /// At `half`, half a ring behind the publisher; no move is refused.
    Live,
    /// At the record `resume` says; a lap to `end` or past it is refused, a step not.
    Drain { end: u64, resume: Resume },
}

/// Where a lapped drain resumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resume {
    /// At `tail`, the oldest record kept.
    Tail {
        /// Where the last resume at `tail` put the reader.
        reached: Option<u64>,
        /// The last lap found the reader still where the resume at `tail` before it put it.
        overtaken: bool,
    },
    /// At `half` for the rest of the drain, after two laps in a row overtook a `tail` resume.
    Half,
}

impl Resume {
    /// A lap at `read`: the second in a row where a resume at `tail` put the reader turns `half`.
    fn overtaken_at(&mut self, read: u64) {
        if let Resume::Tail { reached, overtaken } = *self {
            *self = match (reached == Some(read), overtaken) {
                (false, _) => Resume::Tail {
                    reached,
                    overtaken: false,
                },
                (true, false) => Resume::Tail {
                    reached,
                    overtaken: true,
                },
                (true, true) => Resume::Half,
            };
        }
    }
}

/// A reader's position, the record it owes, and how it resumes.
#[derive(Debug)]
pub(super) struct Cursor {
    read: u64,
    owed: Owed,
    lap: Lap,
}

impl Cursor {
    /// At `read`, a record start; `index` is that record's, when the publisher said which.
    pub(super) fn attached(read: u64, index: Option<u32>) -> Self {
        let owed = match index {
            Some(next) => Owed::Here { next },
            None => Owed::Unanchored,
        };
        Self {
            read,
            owed,
            lap: Lap::Live,
        }
    }

    /// Where the next record to return starts.
    pub(super) fn read(&self) -> u64 {
        self.read
    }

    /// Stop short of `end`, a drain that resumes at `tail` first, or read on with `None`.
    pub(super) fn stop_at(&mut self, end: Option<u64>) {
        self.lap = match end {
            Some(end) => Lap::Drain {
                end,
                resume: Resume::Tail {
                    reached: None,
                    overtaken: false,
                },
            },
            None => Lap::Live,
        };
    }

    /// A lap at the position: whether to resume at `tail`, not `half`; [`Resume`] says when.
    pub(super) fn lapped(&mut self) -> bool {
        match &mut self.lap {
            Lap::Live => false,
            Lap::Drain { resume, .. } => {
                resume.overtaken_at(self.read);
                matches!(resume, Resume::Tail { .. })
            }
        }
    }

    /// Whether `header` here may be believed unjudged: the next index; any while none is known.
    pub(super) fn expects(&self, header: Header) -> bool {
        match self.owed {
            Owed::Unanchored => true,
            Owed::Here { next } => next == header.index,
            Owed::Owing { .. } => false,
        }
    }

    /// Settle on the judged record at the position: how many its index says were lost before it.
    pub(super) fn count_at(&mut self, judged: &Judged) -> u64 {
        debug_assert_eq!(judged.at, self.read, "a count at another record");
        let index = judged.header.index;
        let lost = match self.owed {
            Owed::Unanchored => 0,
            Owed::Here { next } => index.wrapping_sub(next),
            Owed::Owing { next, credited } => index.wrapping_sub(next).saturating_sub(credited),
        };
        self.owed = Owed::Here { next: index };
        u64::from(lost)
    }

    /// Move over the judged record at the position; whether a drain has reached its end.
    pub(super) fn step(&mut self, judged: Judged) -> bool {
        debug_assert_eq!(judged.at, self.read, "a step over another record");
        debug_assert!(
            !matches!(self.owed, Owed::Owing { .. }),
            "a step past a debt"
        );
        self.read = judged.at + judged.size;
        self.owed = Owed::Here {
            next: judged.header.index.wrapping_add(1),
        };
        matches!(self.lap, Lap::Drain { end, .. } if self.read >= end)
    }

    /// Stay on the judged end record, at the position or, found as the newest, past it.
    pub(super) fn rest_on(&mut self, judged: Judged) {
        debug_assert!(judged.at >= self.read, "a reader moved back");
        self.read = judged.at;
        self.owed = Owed::Here {
            next: judged.header.index,
        };
    }

    /// Move past the judged newest record; what lies before it is skipped and owed to nobody.
    pub(super) fn skip_to(&mut self, judged: Judged) {
        debug_assert!(judged.at >= self.read, "a reader moved back");
        self.read = judged.at + judged.size;
        self.owed = Owed::Here {
            next: judged.header.index.wrapping_add(1),
        };
    }

    /// Resume a lapped reader at the record start `to`; `false` when a drain would reach its end.
    pub(super) fn jump(&mut self, to: u64) -> bool {
        // `tail` lies past every byte the judgment found written over, and `half` past `tail`.
        debug_assert!(to > self.read, "a lap resumes ahead of the reader");
        match &mut self.lap {
            Lap::Drain { end, .. } if to >= *end => return false,
            Lap::Drain {
                resume: Resume::Tail { reached, .. },
                ..
            } => *reached = Some(to),
            Lap::Drain { .. } | Lap::Live => {}
        }
        self.read = to;
        if let Owed::Here { next } = self.owed {
            self.owed = Owed::Owing { next, credited: 0 };
        }
        true
    }

    /// A drain's end, while a record owed lies before it.
    pub(super) fn owing_before_end(&self) -> Option<u64> {
        let Lap::Drain { end, .. } = self.lap else {
            return None;
        };
        let owes = !matches!(self.owed, Owed::Unanchored);
        (owes && self.read < end).then_some(end)
    }

    /// Settle on the judged record at a drain's end: the records before it not counted yet.
    pub(super) fn count_to_end(&mut self, judged: Judged) -> u64 {
        debug_assert_eq!(self.owing_before_end(), Some(judged.at));
        self.read = judged.at;
        self.count_at(&judged)
    }

    /// Count the record owed before a drain's end as lost, unless counted already: a lower bound.
    pub(super) fn credit(&mut self) -> u64 {
        debug_assert!(self.owing_before_end().is_some());
        let next = match self.owed {
            Owed::Unanchored | Owed::Owing { credited: 1.., .. } => return 0,
            Owed::Here { next } | Owed::Owing { next, .. } => next,
        };
        self.owed = Owed::Owing { next, credited: 1 };
        1
    }
}
