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
    // ring of `ringBytes`; start() publishes to it as well.
    IBinder topic(int ringBytes);

    // Publishes `count` items of `size` bytes to every consumer added, at
    // `rateHz` (0 = as fast as the slowest consumer takes them), then ends.
    void start(int count, int size, int rateHz);

    // Sends that failed since the last start(); a oneway past the async space fails here.
    int failures();

    BlobBytes echo(in BlobBytes data);
}
