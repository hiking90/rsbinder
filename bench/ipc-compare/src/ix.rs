//! iceoryx2's side, one process per participant as rsbinder's: publish-subscribe (B2, B3) and
//! request-response (B1). `--wait block` pairs each with an event service, the way iceoryx2's
//! examples block; `--wait poll` spins on `receive`.

use std::io::Write;
use std::time::{Duration, Instant};

use iceoryx2::port::backpressure_strategy::BackpressureStrategy;
use iceoryx2::prelude::*;

use crate::common::*;

type Error = Box<dyn std::error::Error>;

/// The longest a blocking subscriber sleeps before it looks again (see `consumer`).
const MISSED_WAKE: Duration = Duration::from_millis(1);

fn ready(word: &str) {
    println!("{word}");
    std::io::stdout().flush().ok();
}

fn node() -> Result<Node<ipc::Service>, Error> {
    set_log_level(LogLevel::Error);
    Ok(NodeBuilder::new().create::<ipc::Service>()?)
}

fn name(base: &str, suffix: &str) -> Result<ServiceName, Error> {
    Ok(ServiceName::new(&format!("{base}{suffix}"))?)
}

/// Samples a subscriber may hold: a few items deep, as `rsb::ring_for` makes the ring.
pub fn buffer_for(size: usize) -> usize {
    (16 * 1024 * 1024 / size.max(1)).clamp(2, 64)
}

/// Child role `ix-producer`: creates the service, waits for `--n` subscribers, publishes.
pub fn producer(a: &Args) -> Result<(), Error> {
    pin(a.num("cpu", 1) as usize);
    let base = a.str("name", "");
    let n = a.num("n", 1) as usize;
    let size = a.num("size", 64) as usize;
    let count = a.num("count", 0);
    let rate = a.num("rate", 0);
    let block = a.str("wait", "block") == "block";
    let node = node()?;
    let service = node
        .service_builder(&name(&base, "")?)
        .publish_subscribe::<[u8]>()
        .max_publishers(1)
        .max_subscribers(n)
        .history_size(0)
        .subscriber_max_buffer_size(buffer_for(size))
        .enable_safe_overflow(false)
        .create()?;
    let publisher = service
        .publisher_builder()
        .initial_max_slice_len(size)
        .backpressure_strategy(BackpressureStrategy::RetryUntilDelivered)
        .create()?;
    let event = node.service_builder(&name(&base, "/ev")?).event().max_listeners(n).create()?;
    let notifier = event.notifier_builder().create()?;
    ready("READY");
    while service.dynamic_config().number_of_subscribers() < n {
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(50));
    let mut buf = vec![0u8; size.max(STAMP_BYTES)];
    let period = if rate > 0 { 1_000_000_000 / rate } else { 0 };
    let start = now_ns() + 1_000_000;
    for seq in 0..count {
        if period > 0 {
            sleep_until(start + seq * period);
        }
        stamp(&mut buf, seq);
        let sample = publisher.loan_slice_uninit(buf.len())?;
        sample.write_from_slice(&buf).send()?;
        if block {
            notifier.notify()?;
        }
    }
    ready("DONE");
    // Exits with its node once the subscribers have, so `/dev/shm` keeps nothing of the run.
    let end = Instant::now() + Duration::from_secs(300);
    while service.dynamic_config().number_of_subscribers() > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Child role `ix-cleanup`: removes what killed iceoryx2 processes left behind.
pub fn cleanup() -> Result<(), Error> {
    let mut dead = Vec::new();
    Node::<ipc::Service>::list(Config::global_config(), |state| {
        if let NodeState::Dead(view) = state {
            dead.push(view);
        }
        CallbackProgression::Continue
    })?;
    let found = dead.len();
    for view in dead {
        view.try_remove_stale_resources()?;
    }
    println!("removed {found} dead iceoryx2 nodes");
    Ok(())
}

/// Child role `ix-consumer`: one subscriber.
pub fn consumer(a: &Args) -> Result<(), Error> {
    pin(a.num("cpu", 2) as usize);
    let base = a.str("name", "");
    let count = a.num("count", 0);
    let block = a.str("wait", "block") == "block";
    let touch = a.str("touch", "no") == "yes";
    let node = node()?;
    let event = node.service_builder(&name(&base, "/ev")?).event().open()?;
    let listener = event.listener_builder().create()?;
    let service = node.service_builder(&name(&base, "")?).publish_subscribe::<[u8]>().open()?;
    let subscriber = service.subscriber_builder().create()?;
    ready("SUBSCRIBED");
    let mut record = Record::new(a.num("warmup", 0));
    let mut idle: Option<Instant> = None;
    // Waits that ended at MISSED_WAKE with a sample there: a notify that never woke this side.
    let mut timed_out = false;
    let mut missed = 0u64;
    while record.received < count {
        if let Some(sample) = subscriber.receive()? {
            missed += u64::from(timed_out);
            timed_out = false;
            if touch {
                // As a reader that keeps the value: one copy out of the sample.
                let copy = sample.payload().to_vec();
                record.take(&copy);
            } else {
                record.take(sample.payload());
            }
            idle = None;
            continue;
        }
        timed_out = false;
        if idle.get_or_insert_with(Instant::now).elapsed() > SILENCE {
            break;
        }
        if block {
            let mut woke = false;
            listener.timed_wait(|_| woke = true, MISSED_WAKE)?;
            timed_out = !woke;
        }
    }
    if missed > 0 {
        record.extra = format!("missed-wakes={missed}");
    }
    println!("{}", record.line());
    Ok(())
}

/// Child role `ix-server`: B1's server, answers each request with a copy of it.
pub fn server(a: &Args) -> Result<(), Error> {
    pin(a.num("cpu", 1) as usize);
    let base = a.str("name", "");
    let size = a.num("size", 8) as usize;
    let block = a.str("wait", "block") == "block";
    let node = node()?;
    let service = node
        .service_builder(&name(&base, "")?)
        .request_response::<[u8], [u8]>()
        .max_clients(1)
        .max_servers(1)
        .create()?;
    let server = service.server_builder().initial_max_slice_len(size).create()?;
    let requests = node.service_builder(&name(&base, "/req")?).event().create()?;
    let responses = node.service_builder(&name(&base, "/resp")?).event().create()?;
    let listener = requests.listener_builder().create()?;
    let notifier = responses.notifier_builder().create()?;
    ready("READY");
    let mut served = false;
    let mut idle = 0u32;
    loop {
        match server.receive()? {
            Some(request) => {
                let response = request.loan_slice_uninit(request.payload().len())?;
                response.write_from_slice(request.payload()).send()?;
                if block {
                    notifier.notify()?;
                }
                served = true;
                continue;
            }
            None if block => {
                listener.timed_wait(|_| {}, Duration::from_millis(10))?;
            }
            None => {
                // A spinning server looks at the client count rarely, to keep its poll short.
                idle = idle.wrapping_add(1);
                if idle % 65536 != 0 {
                    continue;
                }
            }
        }
        // Exits with its node once the client has, as the producer does.
        if served && service.dynamic_config().number_of_clients() == 0 {
            return Ok(());
        }
    }
}

/// Child role `ix-client`: B1's client.
pub fn client(a: &Args) -> Result<(), Error> {
    pin(a.num("cpu", 2) as usize);
    let base = a.str("name", "");
    let size = a.num("size", 8) as usize;
    let count = a.num("count", 0);
    let warmup = a.num("warmup", 0);
    let block = a.str("wait", "block") == "block";
    let node = node()?;
    let service = node.service_builder(&name(&base, "")?).request_response::<[u8], [u8]>().open()?;
    let client = service.client_builder().initial_max_slice_len(size).create()?;
    let requests = node.service_builder(&name(&base, "/req")?).event().open()?;
    let responses = node.service_builder(&name(&base, "/resp")?).event().open()?;
    let notifier = requests.notifier_builder().create()?;
    let listener = responses.listener_builder().create()?;
    let mut buf = vec![0u8; size];
    let mut record = Record::new(0);
    for seq in 0..count {
        stamp(&mut buf, seq);
        let start = now_ns();
        let pending = client.loan_slice_uninit(size)?.write_from_slice(&buf).send()?;
        if block {
            notifier.notify()?;
        }
        let len = loop {
            if let Some(response) = pending.receive()? {
                break response.payload().len();
            }
            if block {
                listener.blocking_wait(|_| {})?;
            }
        };
        let end = now_ns();
        if len != size {
            eprintln!("ix-client: response of {len} bytes");
            break;
        }
        record.received += 1;
        if seq >= warmup {
            if record.first_ns == 0 {
                record.first_ns = start;
            }
            record.hist.saturating_record((end - start).max(1));
            record.last_ns = end;
        }
    }
    println!("{}", record.line());
    Ok(())
}
