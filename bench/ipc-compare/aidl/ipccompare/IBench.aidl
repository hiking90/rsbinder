package ipccompare;

import ipccompare.BlobBytes;
import ipccompare.Config;
import ipccompare.IBenchSink;
import ipccompare.StreamBytes;
import rsbinder.stream.StreamEndpoint;

// The producer process. B2/B3: each consumer adds itself, then the driver
// calls start(); B1: echo().
interface IBench {
    void addStream(in StreamEndpoint<StreamBytes> endpoint);

    void addCallback(IBenchSink sink);

    // The producer's topic (rsbinder-topic), made on the first call with a
    // ring of `ringBytes`; start() publishes to it as well. With
    // `backpressureMs` of 0 or more a publish waits that long for its
    // subscribers (PublisherBuilder::lossless); below 0 it never waits.
    // A `window` above 0 is that of PublisherBuilder::lossless_window; 0 sets none.
    IBinder topic(int ringBytes, int backpressureMs, int window);

    // The same topic with a pool of `slotBytes` slots, for `subscribers`
    // consumers keeping `depth` values; start() lends a slot per item.
    IBinder loanTopic(int ringBytes, int slotBytes, int depth, int subscribers, int backpressureMs,
            int window);

    // Publishes `count` items of `size` bytes to every consumer added, at
    // `rateHz` (0 = as fast as the slowest consumer takes them), then ends.
    void start(int count, int size, int rateHz);

    // Sends that failed since the last start(); a oneway past the async space fails here.
    int failures();

    BlobBytes echo(in BlobBytes data);

    // B1 with the payload in the transaction itself, whatever its size.
    byte[] echoBytes(in byte[] data);

    // B8: a call with a parcelable of several kinds of field, returned as it came.
    Config describe(in Config config);

    // B10: a value read with a call, and the same value as a state topic.
    long value();
    IBinder stateTopic();

    // B11: maps `len` bytes of `fd`, and returns the sequence number stamped in them plus the
    // last byte.
    long touch(in ParcelFileDescriptor fd, long len);

    // B12: a region the client shares once; echoShared() copies `len` bytes of it out and
    // writes them back.
    void shareRegion(in ParcelFileDescriptor fd);
    void echoShared(long len);

    // B12: an IMemory of the client's dealer in, its bytes in an IMemory of the server's
    // dealer back.
    IBinder echoMemory(IBinder memory);
}
