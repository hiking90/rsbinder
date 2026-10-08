// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fails when `loom_event_flag.rs`'s copy of `EventFlag` drifts from `src/event_flag.rs`.

fn read(path: &str) -> String {
    let path = format!("{}/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The body of `fn name(`, comments stripped.
fn body(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("no `fn {name}(`"));
    let open = start + source[start..].find('{').expect("a body");
    let mut depth = 0;
    let end = source[open..]
        .char_indices()
        .find_map(|(i, c)| {
            match c {
                '{' => depth += 1,
                '}' if depth == 1 => return Some(open + i),
                '}' => depth -= 1,
                _ => {}
            }
            None
        })
        .expect("a closed body");
    source[open..end]
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fences, atomic accesses with their orderings, futex calls and `wake` calls, in source order.
fn protocol(body: &str) -> Vec<String> {
    const CALLS: [&str; 8] = [
        "fence(",
        ".fetch_or(",
        ".fetch_and(",
        ".load(",
        ".store(",
        "futex_wait(",
        "futex_wake(",
        "self.wake(",
    ];
    let mut out = Vec::new();
    for (at, _) in body.char_indices() {
        let rest = &body[at..];
        let Some(call) = CALLS.iter().find(|c| rest.starts_with(**c)) else {
            continue;
        };
        let statement = rest.split([';', '\n']).next().unwrap_or(rest);
        let ordering = statement
            .split_once("Ordering::")
            .map(|(_, o)| o.split(|c: char| !c.is_alphanumeric()).next().unwrap_or(""));
        let name = call.trim_matches(|c: char| c == '.' || c == '(');
        out.push(match ordering {
            Some(o) => format!("{name}:{o}"),
            None => name.to_string(),
        });
    }
    out
}

#[test]
fn the_loom_model_keeps_event_flags_atomics() {
    let real = read("src/event_flag.rs");
    let model = read("tests/loom_event_flag.rs");
    for (in_model, in_real) in [
        ("wake", "wake"),
        ("wake_lazy", "wake_lazy"),
        ("wait", "wait_until"),
    ] {
        let expected = protocol(&body(&real, in_real));
        assert!(
            !expected.is_empty(),
            "`{in_real}` has no atomics to compare"
        );
        assert_eq!(
            protocol(&body(&model, in_model)),
            expected,
            "the loom model's `{in_model}` no longer matches `EventFlag::{in_real}`"
        );
    }
}
