package ipccompare;

// A client of the `load` set: the binder a service calls back into and watches.
interface ILoadSink {
    byte[] echo(in byte[] data);
}
