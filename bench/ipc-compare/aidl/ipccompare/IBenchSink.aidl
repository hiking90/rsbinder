package ipccompare;

// A consumer of B3's "oneway callback x N".
interface IBenchSink {
    oneway void deliver(in byte[] data);
}
