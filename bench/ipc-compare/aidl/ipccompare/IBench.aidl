package ipccompare;

import ipccompare.BlobBytes;
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
    // subscribers (PublisherPolicy::backpressure); below 0 it never waits.
    // A `window` above 0 is Backpressure::window; 0 sets none.
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
}
