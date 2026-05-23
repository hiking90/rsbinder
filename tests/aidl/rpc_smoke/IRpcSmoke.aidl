// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Minimal AIDL fixture for subplan 2-6.B: drives the *generated* Bp*
// stub (which now emits `as_remote().ok_or(BadType)?`) over the RPC
// transport — scalar, string, and a oneway method to exercise the
// generator's prepare_transact / submit_transact / FLAG_ONEWAY paths.
package rpcsmoke;

interface IRpcSmoke {
    String echo(String s);
    int add(int a, int b);
    oneway void ping();
}
