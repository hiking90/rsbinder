// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The two ways a binary here writes to stderr: as a command or as a daemon.
//!
//! A **command** (`rsb_device`, `rsb_service`) runs once for someone at a
//! terminal or in a script. It reports its own errors itself, as
//! `rsb_device: <message>`, and turns the `log` records of the libraries it
//! calls off unless `RUST_LOG` asks for them: those records repeat, in a
//! second format, the failure the command already reports.
//!
//! A **daemon** (`rsb_hub`) runs unattended, and `log` is how it says
//! anything, failures to start included; `rsb_hub` writes one that
//! `RUST_LOG` filters out as plain text rather than drop it. On a terminal it writes
//! `env_logger`'s format. When systemd connects its stderr to the journal it
//! writes each line as `<N>message`, the `sd-daemon(3)` priority prefix: the
//! journal adds its own timestamp and process identifier, and `<N>` is what
//! sets the entry's priority, so `journalctl -p err` finds the hub's errors.
//! Without the prefix every line is stored at the unit's default priority.

use std::io::Write;

use env_logger::{Builder, Env};
use log::{Level, LevelFilter};

/// Logging for a command: library records only when `RUST_LOG` names them.
pub fn init_command() {
    Builder::new()
        .filter_level(LevelFilter::Off)
        .parse_default_env()
        .init();
}

/// Logging for a daemon whose own records carry the target `own_target`.
///
/// `default_filter` applies when `RUST_LOG` is unset. In the journal, a
/// record from any other target is prefixed with that target, which the
/// terminal format shows on every line anyway.
pub fn init_daemon(own_target: &'static str, default_filter: &str) {
    let mut builder = Builder::from_env(Env::default().default_filter_or(default_filter));
    if stderr_is_the_journal() {
        builder.format(move |buf, record| {
            let target = (record.target() != own_target).then(|| record.target());
            let lines = journal_lines(record.level(), target, &record.args().to_string());
            buf.write_all(lines.as_bytes())
        });
    }
    builder.init();
}

/// systemd sets `JOURNAL_STREAM` to the `<dev>:<ino>` of the unit's stream (`systemd.exec(5)`).
#[expect(
    clippy::unnecessary_cast,
    reason = "st_dev is i32 (dev_t) on macOS; the fields are u64 or c_ulong = u64 elsewhere"
)]
fn stderr_is_the_journal() -> bool {
    let Ok(stream) = std::env::var("JOURNAL_STREAM") else {
        return false;
    };
    // A child that inherits the variable but not the stream must not take the prefix.
    rustix::fs::fstat(std::io::stderr())
        .is_ok_and(|stat| names_the_stream(&stream, stat.st_dev as u64, stat.st_ino as u64))
}

fn names_the_stream(stream: &str, dev: u64, ino: u64) -> bool {
    stream
        .split_once(':')
        .and_then(|(d, i)| Some((d.parse::<u64>().ok()?, i.parse::<u64>().ok()?)))
        == Some((dev, ino))
}

/// The journal makes an entry of each line, so each carries the prefix.
fn journal_lines(level: Level, target: Option<&str>, message: &str) -> String {
    let priority = match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
    };
    // A message that ends in `\n` (toml's errors do) would otherwise end in an empty entry.
    let message = message.strip_suffix('\n').unwrap_or(message);
    let mut out = String::new();
    for (index, line) in message.split('\n').enumerate() {
        out.push_str(&format!("<{priority}>"));
        if let (0, Some(target)) = (index, target) {
            out.push_str(&format!("{target}: "));
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_journal_stream_is_matched_by_both_numbers() {
        assert!(names_the_stream("8:12345", 8, 12345));
        assert!(!names_the_stream("8:12345", 8, 12346));
        assert!(!names_the_stream("9:12345", 8, 12345));
        assert!(!names_the_stream("8", 8, 12345));
        assert!(!names_the_stream("8:x", 8, 12345));
        assert!(!names_the_stream("", 0, 0));
    }

    #[test]
    fn every_line_takes_the_priority_and_the_first_the_target() {
        assert_eq!(
            journal_lines(Level::Error, None, "rsb_hub: no config"),
            "<3>rsb_hub: no config\n"
        );
        assert_eq!(
            journal_lines(Level::Warn, Some("rsbinder::process_state"), "a\n\n  b"),
            "<4>rsbinder::process_state: a\n<4>\n<4>  b\n"
        );
        assert_eq!(journal_lines(Level::Error, None, "x\n"), "<3>x\n");
        assert_eq!(journal_lines(Level::Info, None, "x"), "<6>x\n");
        assert_eq!(journal_lines(Level::Debug, None, "x"), "<7>x\n");
        assert_eq!(journal_lines(Level::Trace, None, "x"), "<7>x\n");
    }
}
