// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A state topic beside the methods a topic does not have: a thermostat whose target temperature
//! is published on a state topic, read once through a getter, and changed through a setter. With
//! `rsb_hub` running:
//!
//! ```text
//! cargo run -p rsbinder-topic --example topic_thermostat -- serve
//! cargo run -p rsbinder-topic --example topic_thermostat -- watch
//! cargo run -p rsbinder-topic --example topic_thermostat -- set 23.5
//! cargo run -p rsbinder-topic --example topic_thermostat -- get
//! ```
//!
//! `watch` prints the current target as it subscribes and every later one; `set` prints the target
//! the service settled on, which is 30 for anything above 30.

use std::sync::Mutex;

use rsbinder::{
    hub, BinderResult, ExceptionCode, Interface, ProcessState, SIBinder, Status, Strong,
};
use rsbinder_topic::{Publisher, Subscriber};

const NAME: &str = "example.topic.thermostat";
const COLDEST: f64 = 10.0;
const WARMEST: f64 = 30.0;

// ANCHOR: value
#[derive(rsbinder::Parcelable, Debug, Default, Clone, Copy, PartialEq)]
#[parcelable(descriptor = "example.topic.Target")]
struct Target {
    celsius: f64,
}
// ANCHOR_END: value

// ANCHOR: interface
#[rsbinder::interface(descriptor = "example.topic.IThermostat")]
trait IThermostat {
    /// The target, for a client that does not subscribe.
    fn get_target(&self) -> BinderResult<f64>;
    /// Ask for a target; returns the one the service settled on.
    fn set_target(&self, celsius: f64) -> BinderResult<f64>;
    /// The state topic of the target, for `SubscriberBuilder::subscribe`.
    fn topic(&self) -> BinderResult<SIBinder>;
}
// ANCHOR_END: interface

// ANCHOR: service
struct Thermostat {
    target: Mutex<Target>,
    topic: Publisher<Target>,
}

impl Thermostat {
    fn new(first: Target) -> BinderResult<Self> {
        Ok(Self {
            target: Mutex::new(first),
            // The topic has the service's first value from before anyone can reach it.
            topic: Publisher::state().build(&first)?,
        })
    }
}

impl Interface for Thermostat {}

impl IThermostat for Thermostat {
    fn get_target(&self) -> BinderResult<f64> {
        Ok(self.target.lock().expect("the target's lock").celsius)
    }

    fn set_target(&self, celsius: f64) -> BinderResult<f64> {
        if celsius.is_nan() {
            return Err(Status::from((
                ExceptionCode::IllegalArgument,
                "not a number",
            )));
        }
        let settled = Target {
            celsius: celsius.clamp(COLDEST, WARMEST),
        };
        // One lock over both: two setters leave the getter and the topic with the same value.
        let mut target = self.target.lock().expect("the target's lock");
        self.topic
            .publish(&settled)
            .map_err(|why| Status::from((ExceptionCode::IllegalState, why.to_string().as_str())))?;
        *target = settled;
        // Published before the reply: the caller's next receive has this value or a newer one.
        Ok(settled.celsius)
    }

    fn topic(&self) -> BinderResult<SIBinder> {
        Ok(self.topic.as_binder())
    }
}
// ANCHOR_END: service

fn thermostat() -> Result<Strong<dyn IThermostat>, Box<dyn std::error::Error>> {
    let binder = hub::check_service(NAME).ok_or("no thermostat is serving")?;
    Ok(binder.into_interface()?)
}

// ANCHOR: watch
fn watch() -> Result<(), Box<dyn std::error::Error>> {
    let topic = thermostat()?.topic()?;
    let mut subscriber = Subscriber::<Target>::builder().subscribe(&topic)?;
    loop {
        // The first receive returns the current target; a slow reader skips to the newest.
        let target = subscriber.recv()?;
        println!("target: {} °C", target.celsius);
    }
}
// ANCHOR_END: watch

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {
            let service = Thermostat::new(Target { celsius: 21.0 })?;
            hub::add_service(NAME, BnThermostat::new_binder(service).as_binder())?;
            Ok(ProcessState::join_thread_pool()?)
        }
        Some("watch") => watch(),
        Some("get") => {
            println!("target: {} °C", thermostat()?.get_target()?);
            Ok(())
        }
        Some("set") => {
            let celsius = args.next().ok_or("set takes a temperature")?.parse()?;
            println!("target: {} °C", thermostat()?.set_target(celsius)?);
            Ok(())
        }
        _ => Err("usage: topic_thermostat serve|watch|get|set <celsius>".into()),
    }
}
