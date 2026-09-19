// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-9 fixture: an interface generated with `Builder::trace(true)`,
// so its service carries a method-name table (`tests/transaction_names.rs`),
// and what a transaction observer saw can be read back (`observed`) by
// `tests/observer_rpc.rs` and `observer_probe`.
package tracedemo;

interface ITraceDemo {
    void ping();
    int add(int a, int b);
    oneway void notify(String s);
    List<String> observed();
}
