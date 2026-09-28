// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Fixture for the builtin `android.hardware.common` types and generic
// parcelables (plan 12 F0): the imports below have no `.aidl` in this
// crate — rsbinder-aidl resolves them to `rsbinder::fmq::*` and
// `rsbinder::NativeHandle`. `tests/tests/fmq_parcel.rs` drives it.
package fmqdemo;

import android.hardware.common.fmq.MQDescriptor;
import android.hardware.common.fmq.SynchronizedReadWrite;
import android.hardware.common.fmq.UnsynchronizedWrite;
import android.hardware.common.NativeHandle;

// A generic parcelable of this crate's own: `Tag` is a phantom that never
// reaches the parcel.
@RustDerive(Clone=true, PartialEq=true)
parcelable Tagged<Tag> {
    int id;
    String label;
}

@VintfStability
parcelable QueueBundle {
    MQDescriptor<int, SynchronizedReadWrite> queue;
    @nullable MQDescriptor<byte, UnsynchronizedWrite> overflow;
    NativeHandle extra;
}

interface IFmqDemo {
    MQDescriptor<byte, SynchronizedReadWrite> openQueue(int capacity);
    void closeQueue(in MQDescriptor<byte, SynchronizedReadWrite> desc);
    QueueBundle bundle();
    Tagged<QueueBundle> tag(in Tagged<QueueBundle> value, in Tagged<int>[] others);
}
