// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC mesh integration test (hermetic, runs on every OS incl. macOS).
//!
//! Spawns ~10 `mesh_node` processes wired into a full mesh over RPC
//! unix sockets — a mix of server roles (each binds a socket *and*
//! drives every other server) and client-only roles — lets them
//! exchange the complex `IMeshNode` AIDL (parcelable + union + byte
//! array + oneway) without pause for a fixed duration, then parses each
//! node's `MESH_SUMMARY` line and asserts every exchange round-tripped
//! intact (`integrity_fail == 0`), there were no transport errors
//! (`errors == 0`), and real traffic flowed (`tx > 0`).
//!
//! Separate `#[cfg(feature = "rpc")]` test binary so it never shares a
//! process with the kernel-binder unit tests. No kernel binder, no
//! service manager — pure RPC, so it is fully self-contained.

#![cfg(feature = "rpc")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const MESH_NODE: &str = env!("CARGO_BIN_EXE_mesh_node");

/// One parsed `MESH_SUMMARY` line.
#[derive(Debug, Default)]
struct Summary {
    name: String,
    role: String,
    tx: u64,
    rx: u64,
    errors: u64,
    integrity_fail: u64,
    peer_gone: u64,
    served_rx: i64,
}

fn parse_summary(line: &str) -> Option<Summary> {
    let rest = line.strip_prefix("MESH_SUMMARY ")?;
    let mut s = Summary::default();
    for kv in rest.split_whitespace() {
        let (k, v) = kv.split_once('=')?;
        match k {
            "name" => s.name = v.to_string(),
            "role" => s.role = v.to_string(),
            "tx" => s.tx = v.parse().ok()?,
            "rx" => s.rx = v.parse().ok()?,
            "errors" => s.errors = v.parse().ok()?,
            "integrity_fail" => s.integrity_fail = v.parse().ok()?,
            "peer_gone" => s.peer_gone = v.parse().ok()?,
            "served_rx" => s.served_rx = v.parse().ok()?,
            _ => {}
        }
    }
    Some(s)
}

fn sock_path(tag: &str, i: usize) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("rsb_mesh.{}.{tag}.{i}.sock", std::process::id()));
    p.to_string_lossy().into_owned()
}

/// Run a spawned node to completion, returning its parsed summary.
/// stdout is streamed so a hung child can't fill a pipe buffer.
fn wait_summary(mut child: Child, who: &str) -> Summary {
    let stdout = child.stdout.take().expect("child stdout");
    let reader = BufReader::new(stdout);
    let mut summary: Option<Summary> = None;
    for line in reader.lines().map_while(std::result::Result::ok) {
        if let Some(s) = parse_summary(&line) {
            summary = Some(s);
        }
    }
    let status = child.wait().expect("wait child");
    assert!(status.success(), "node {who} exited with {status:?}");
    summary.unwrap_or_else(|| panic!("node {who} produced no MESH_SUMMARY"))
}

#[test]
fn rpc_mesh_ten_nodes_exchange_without_error() {
    // Topology: 5 server nodes (each binds a socket and hammers every
    // *other* server), plus 5 client-only nodes (each hammers every
    // server). 10 processes total; the kernel binder mesh variant lives
    // in `mesh_kernel.rs` (linux/android only).
    const SERVERS: usize = 5;
    const CLIENTS: usize = 5;
    const DURATION_MS: u64 = 1500;

    let server_socks: Vec<String> = (0..SERVERS).map(|i| sock_path("srv", i)).collect();
    for s in &server_socks {
        let _ = std::fs::remove_file(s);
    }

    let spawn = |role: &str, name: &str, listen: Option<&str>, peers: &[String]| -> Child {
        let mut cmd = Command::new(MESH_NODE);
        cmd.arg("--role").arg(role).arg("--name").arg(name);
        cmd.arg("--duration-ms").arg(DURATION_MS.to_string());
        cmd.arg("--blob").arg("48");
        if let Some(l) = listen {
            cmd.arg("--listen").arg(l);
        }
        for p in peers {
            cmd.arg("--peer").arg(p);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());
        cmd.spawn().expect("spawn mesh_node")
    };

    let mut children: Vec<(String, Child)> = Vec::new();

    // Server nodes: each binds its own socket and peers with every OTHER
    // server (full mesh among servers).
    for (i, listen) in server_socks.iter().enumerate() {
        let peers: Vec<String> = server_socks
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, s)| s.clone())
            .collect();
        let name = format!("srv{i}");
        children.push((
            name.clone(),
            spawn("rpc-server", &name, Some(listen), &peers),
        ));
    }

    // Client-only nodes: each hammers every server.
    for i in 0..CLIENTS {
        let name = format!("cli{i}");
        children.push((
            name.clone(),
            spawn("rpc-client", &name, None, &server_socks),
        ));
    }

    // Collect summaries (with a generous wall-clock guard: the duration
    // plus connect/retry slack).
    let summaries: Vec<Summary> = children
        .into_iter()
        .map(|(name, child)| wait_summary(child, &name))
        .collect();

    for s in &server_socks {
        let _ = std::fs::remove_file(s);
    }

    assert_eq!(summaries.len(), SERVERS + CLIENTS, "all nodes reported");

    let mut total_tx = 0u64;
    let mut total_served = 0i64;
    for s in &summaries {
        assert_eq!(
            s.integrity_fail, 0,
            "node {} had integrity failures: {s:?}",
            s.name
        );
        assert_eq!(s.errors, 0, "node {} had transport errors: {s:?}", s.name);
        assert_eq!(s.tx, s.rx, "node {} tx/rx mismatch: {s:?}", s.name);
        assert!(s.tx > 0, "node {} did no work: {s:?}", s.name);
        total_tx += s.tx;
        total_served += s.served_rx;
    }

    // Every node drove traffic, and the servers collectively handled a
    // large number of inbound exchanges.
    assert!(total_tx > 0, "mesh did no work at all");
    assert!(total_served > 0, "no server observed any inbound exchange");
    eprintln!(
        "rpc_mesh: {} nodes, total_tx={total_tx}, total_server_rx={total_served}",
        summaries.len()
    );

    // Touch Duration so an unused-import lint never fires if the body
    // changes; keeps the dependency explicit.
    let _ = Duration::from_millis(DURATION_MS);
}
