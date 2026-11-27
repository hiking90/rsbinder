package ipccompare;

import ipccompare.BlobBytes;
import ipccompare.ILoadSink;

// A service of the `load` set: several in each server process, called by many clients at once.
interface ILoad {
    // The payload back as it came, in the transaction.
    byte[] echo(in byte[] data);

    // The same through Parcel::write_blob: a memfd past 16 KiB.
    BlobBytes echoBlob(in BlobBytes data);

    // Checked and counted; counters() reports what arrived.
    oneway void post(in byte[] data);

    // Calls sink.echo(data) from inside this call and returns its reply: a transaction into the
    // caller, nested in the caller's own.
    byte[] callBack(ILoadSink sink, in byte[] data);

    // echo() on the service of the same index in the next server process, from inside this
    // call. The last server process has none to call.
    byte[] relay(in byte[] data);

    // Links to the death of `sink`; counters() reports the deaths.
    void watch(ILoadSink sink);

    // Posts received, posts that failed the check, relays and callbacks that failed, watches,
    // deaths delivered, and watches whose death has yet to arrive; then for the whole process,
    // its live heap allocations and bytes and the highest proxy handle a sink came in on.
    long[] counters();

    // Ends the server process with exit(0), so that a tool it runs under writes its report.
    oneway void quit();
}
