// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Kernel-binder + RPC mixed mesh integration test.
//!
//! Spawns ~10 `mesh_node` processes where exactly ONE is the kernel
//! binder service (registered with the service manager) and the other
//! nine are kernel binder clients of it — while, orthogonally, several
//! nodes ALSO front RPC unix-socket endpoints and drive each other over
//! RPC. So the same `IMeshNode` AIDL flows over both transports
//! simultaneously, in one ~10-process mesh, for a fixed duration; each
//! node's `MESH_SUMMARY` is then checked for zero errors and zero
//! integrity failures.
//!
//! Requires a real kernel binder device (`/dev/binder`, Linux+binderfs
//! or Android) and, for the service-manager `addService` of an arbitrary
//! name, a permissive enough environment. Hence `#[ignore]`: run
//! explicitly on a binder-capable host, e.g. the Android emulator:
//!
//! ```text
//! mesh_kernel_and_rpc_mixed_mesh -- --ignored --nocapture
//! ```
//!
//! `#[cfg(feature = "rpc")]` because the mixed nodes front RPC too;
//! `#[cfg(any(linux, android))]` because the kernel roles only compile
//! there.

#![cfg(all(feature = "rpc", any(target_os = "linux", target_os = "android")))]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

// The `mesh_node` binary. Defaults to the cargo-provided path, but can
// be overridden via `MESH_NODE_BIN` so the test can run on a device
// (e.g. the Android emulator) where the build-time path does not exist.
fn mesh_node_bin() -> String {
    std::env::var("MESH_NODE_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_mesh_node").to_string())
}

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
    p.push(format!("rsb_kmesh.{}.{tag}.{i}.sock", std::process::id()));
    p.to_string_lossy().into_owned()
}

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
#[ignore = "requires kernel binder (/dev/binder) + permissive service manager; run on emulator/REMOTE_LINUX"]
fn kernel_and_rpc_mixed_mesh() {
    // Topology (20 processes), deliberately transport-mixed:
    //   - 1 kernel-server: registers `mesh.kernel.<pid>` with the service
    //     manager, AND acts as an RPC client of every RPC endpoint.
    //   - 14 kernel-clients: look up + hammer the kernel service over
    //     kernel binder; HALF of them (7) ALSO front their own RPC
    //     server in the same process — i.e. one process driving kernel
    //     binder and RPC simultaneously — and every kernel-client also
    //     dials the RPC sub-mesh as a client.
    //   - 5 dedicated rpc-servers: pure RPC, full mesh among themselves.
    // So the same IMeshNode AIDL flows over kernel binder and RPC at the
    // same time, with 8 processes (ksrv + 7 kcli) bridging both stacks.
    const KERNEL_CLIENTS: usize = 14;
    const KC_WITH_RPC: usize = 7; // kernel-clients that also serve RPC
    const RPC_SERVERS: usize = 5;
    // Each node runs for this long. Generous enough that all 20
    // independently-launched processes finish kernel ProcessState init +
    // binder registration + RPC server bring-up and FULLY connect the
    // mixed mesh (kernel init load can delay an RPC server's bind well
    // past a shorter window), then exchange for the remainder. The node
    // reserves ~20% of the window for connecting before hammering.
    const DURATION_MS: u64 = 25000;

    let kernel_service = format!("mesh.kernel.{}", std::process::id());

    // RPC endpoints: 5 dedicated rpc-server nodes + 7 kernel-clients that
    // also listen (the transport-bridging processes).
    let rpc_socks: Vec<String> = (0..RPC_SERVERS).map(|i| sock_path("rpc", i)).collect();
    let kc_listen: Vec<String> = (0..KC_WITH_RPC).map(|i| sock_path("kc", i)).collect();
    for s in rpc_socks.iter().chain(kc_listen.iter()) {
        let _ = std::fs::remove_file(s);
    }
    // The full set of RPC endpoints peers can dial.
    let all_rpc: Vec<String> = rpc_socks.iter().chain(kc_listen.iter()).cloned().collect();

    let spawn = |role: &str,
                 name: &str,
                 listen: Option<&str>,
                 peers: &[String],
                 kservice: Option<&str>|
     -> Child {
        let mut cmd = Command::new(mesh_node_bin());
        cmd.arg("--role").arg(role).arg("--name").arg(name);
        cmd.arg("--duration-ms").arg(DURATION_MS.to_string());
        cmd.arg("--blob").arg("48");
        if let Some(l) = listen {
            cmd.arg("--listen").arg(l);
        }
        if let Some(k) = kservice {
            cmd.arg("--kernel-service").arg(k);
        }
        for p in peers {
            cmd.arg("--peer").arg(p);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());
        cmd.spawn().expect("spawn mesh_node")
    };

    let mut children: Vec<(String, Child)> = Vec::new();

    // 1 kernel-server (also an RPC client of all RPC endpoints).
    children.push((
        "ksrv".to_string(),
        spawn(
            "kernel-server",
            "ksrv",
            None,
            &all_rpc,
            Some(&kernel_service),
        ),
    ));

    // Dedicated RPC servers (full mesh among all RPC endpoints).
    for (i, listen) in rpc_socks.iter().enumerate() {
        let peers: Vec<String> = all_rpc.iter().filter(|s| *s != listen).cloned().collect();
        let name = format!("rpc{i}");
        children.push((
            name.clone(),
            spawn("rpc-server", &name, Some(listen), &peers, None),
        ));
    }

    // Let the pure-RPC listeners bind before the kernel-init wave starts,
    // then stagger the kernel-clients: spawning 15 processes that each
    // run ProcessState init + binder ioctls simultaneously saturates the
    // CPU and briefly starves every accept loop (transient ECONNREFUSED).
    // Spreading the launch keeps the mesh fully connectable.
    std::thread::sleep(Duration::from_millis(300));

    // Kernel-clients of the kernel service. The first KC_WITH_RPC also
    // front their own RPC server (bridging both stacks in one process);
    // all of them also dial the RPC sub-mesh as clients.
    for i in 0..KERNEL_CLIENTS {
        let name = format!("kcli{i}");
        let listen = kc_listen.get(i).map(|s| s.as_str());
        // Peers: every RPC endpoint except one's own listen socket.
        let peers: Vec<String> = all_rpc
            .iter()
            .filter(|s| Some(s.as_str()) != listen)
            .cloned()
            .collect();
        children.push((
            name.clone(),
            spawn(
                "kernel-client",
                &name,
                listen,
                &peers,
                Some(&kernel_service),
            ),
        ));
        // Spread the kernel-init load across the launch: spawning many
        // ProcessState-init processes at once saturates the CPU and
        // briefly starves every RPC accept loop.
        std::thread::sleep(Duration::from_millis(150));
    }

    let summaries: Vec<Summary> = children
        .into_iter()
        .map(|(name, child)| wait_summary(child, &name))
        .collect();

    for s in rpc_socks.iter().chain(kc_listen.iter()) {
        let _ = std::fs::remove_file(s);
    }

    assert_eq!(
        summaries.len(),
        1 + RPC_SERVERS + KERNEL_CLIENTS,
        "all nodes reported"
    );

    let mut total_tx = 0u64;
    let mut total_served = 0i64;
    for s in &summaries {
        assert_eq!(
            s.integrity_fail, 0,
            "node {} integrity failures: {s:?}",
            s.name
        );
        assert_eq!(s.errors, 0, "node {} transport errors: {s:?}", s.name);
        assert_eq!(s.tx, s.rx, "node {} tx/rx mismatch: {s:?}", s.name);
        total_tx += s.tx;
        total_served += s.served_rx;
    }

    // The kernel service node must have observed real inbound traffic
    // from its kernel clients.
    let ksrv = summaries
        .iter()
        .find(|s| s.name == "ksrv")
        .expect("ksrv summary");
    assert!(
        ksrv.served_rx > 0,
        "kernel service observed no inbound exchanges: {ksrv:?}"
    );

    assert!(total_tx > 0, "mesh did no work");
    eprintln!(
        "kernel_mesh: {} nodes, total_tx={total_tx}, total_server_rx={total_served}, ksrv_served={}",
        summaries.len(),
        ksrv.served_rx
    );
}
