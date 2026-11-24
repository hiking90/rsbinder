// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A service whose state is a list too large to publish whole on every change: a method returns
//! the list, an event topic carries each change, and a counter joins the two. The service takes
//! a name of the session bus. With `rsb_hub` running:
//!
//! ```text
//! cargo run -p rsbinder-topic --example topic_service -- serve
//! cargo run -p rsbinder-topic --example topic_service -- watch
//! cargo run -p rsbinder-topic --example topic_service -- start backup
//! cargo run -p rsbinder-topic --example topic_service -- stop backup
//! ```
//!
//! `watch` prints the units as it starts and every change after, and lists again whenever it
//! lost changes.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use rsbinder::{
    bus, BinderResult, ExceptionCode, Interface, ProcessState, SIBinder, Status, Strong,
};
use rsbinder_topic::{Publisher, RecvError, Subscriber};

const NAME: &str = "example.topic.units";

// ANCHOR: values
#[derive(rsbinder::Parcelable, Debug, Default, Clone, PartialEq)]
#[parcelable(descriptor = "example.topic.Unit")]
struct Unit {
    name: String,
    active: bool,
}

/// What `list` returns: every unit, and the revision the list is of.
#[derive(rsbinder::Parcelable, Debug, Default, Clone, PartialEq)]
#[parcelable(descriptor = "example.topic.Units")]
struct Units {
    revision: i64,
    units: Vec<Unit>,
}

/// One record of the topic: the unit as the change left it, and the revision the change made.
#[derive(rsbinder::Parcelable, Debug, Default, Clone, PartialEq)]
#[parcelable(descriptor = "example.topic.UnitChange")]
struct UnitChange {
    revision: i64,
    unit: Unit,
}
// ANCHOR_END: values

// ANCHOR: interface
#[rsbinder::interface(descriptor = "example.topic.IUnits")]
trait IUnits {
    /// Every unit, with the revision of the last change the list holds.
    fn list(&self) -> BinderResult<Units>;
    /// Start or stop a unit; a unit not seen before is added.
    fn set_active(&self, name: &str, active: bool) -> BinderResult<()>;
    /// The event topic of the changes, for `SubscriberBuilder::subscribe`.
    fn changes(&self) -> BinderResult<SIBinder>;
}
// ANCHOR_END: interface

// ANCHOR: service
#[derive(Default)]
struct State {
    revision: i64,
    units: BTreeMap<String, bool>,
}

struct UnitsService {
    state: Mutex<State>,
    changes: Publisher<UnitChange>,
}

impl Interface for UnitsService {}

impl IUnits for UnitsService {
    fn list(&self) -> BinderResult<Units> {
        // The same lock as a change: the revision is that of the last change in the list.
        let state = self.state.lock().expect("the state's lock");
        Ok(Units {
            revision: state.revision,
            units: state
                .units
                .iter()
                .map(|(name, &active)| Unit {
                    name: name.clone(),
                    active,
                })
                .collect(),
        })
    }

    fn set_active(&self, name: &str, active: bool) -> BinderResult<()> {
        // One lock over the change, its revision and its publish: records are in revision order.
        let mut state = self.state.lock().expect("the state's lock");
        if state.units.get(name) == Some(&active) {
            return Ok(());
        }
        let change = UnitChange {
            revision: state.revision + 1,
            unit: Unit {
                name: name.to_owned(),
                active,
            },
        };
        self.changes
            .publish(&change)
            .map_err(|why| Status::from((ExceptionCode::IllegalState, why.to_string().as_str())))?;
        state.revision = change.revision;
        state.units.insert(change.unit.name, active);
        Ok(())
    }

    fn changes(&self) -> BinderResult<SIBinder> {
        Ok(self.changes.as_binder())
    }
}
// ANCHOR_END: service

fn units(session: &bus::Bus) -> Result<Strong<dyn IUnits>, Box<dyn std::error::Error>> {
    Ok(session.wait_for_interface(NAME, Duration::from_secs(5))?)
}

// ANCHOR: watch
fn watch(service: &Strong<dyn IUnits>) -> Result<(), Box<dyn std::error::Error>> {
    // Subscribed before the list is asked for, so no change falls between the two.
    let mut changes = Subscriber::<UnitChange>::builder().subscribe(&service.changes()?)?;
    loop {
        let listed = service.list()?;
        println!("revision {}:", listed.revision);
        for unit in &listed.units {
            println!(
                "  {} {}",
                unit.name,
                if unit.active { "active" } else { "inactive" }
            );
        }
        loop {
            match changes.recv() {
                // The list already holds this change.
                Ok(change) if change.revision <= listed.revision => {}
                Ok(UnitChange { revision, unit }) => {
                    let state = if unit.active { "active" } else { "inactive" };
                    println!("revision {revision}: {} {state}", unit.name);
                }
                // Changes were overwritten unread: what they said is in a new list.
                Err(RecvError::Lagged(lost)) => {
                    println!("lost {lost} changes, listing again");
                    break;
                }
                Err(why) => return Err(why.into()),
            }
        }
    }
}
// ANCHOR_END: watch

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: session
    let session = bus::session()?;
    ProcessState::start_thread_pool();
    // ANCHOR_END: session
    let mut args = std::env::args().skip(1);
    let verb = args.next();
    match verb.as_deref() {
        Some("serve") => {
            // ANCHOR: serve
            let service = UnitsService {
                state: Mutex::default(),
                changes: Publisher::events().max_value_bytes(256).keep(64).build()?,
            };
            session.add(NAME, BnUnits::new_binder(service).as_binder())?;
            // ANCHOR_END: serve
            Ok(ProcessState::join_thread_pool()?)
        }
        Some("watch") => watch(&units(&session)?),
        Some(verb @ ("start" | "stop")) => {
            let name = args.next().ok_or("start and stop take a unit's name")?;
            Ok(units(&session)?.set_active(&name, verb == "start")?)
        }
        _ => Err("usage: topic_service serve|watch|start <unit>|stop <unit>".into()),
    }
}
