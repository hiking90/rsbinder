package ipccompare;

import ipccompare.Limits;

// B8's argument and reply: scalars, a string, two arrays and a nested parcelable. The D-Bus
// side sends the same fields as `(ixdbsasai(iid))`, iceoryx2 as a `repr(C)` struct of fixed
// arrays.
parcelable Config {
    int id;
    long timestamp;
    double gain;
    boolean enabled;
    @utf8InCpp String name;
    @utf8InCpp String[] tags;
    int[] samples;
    Limits limits;
}
