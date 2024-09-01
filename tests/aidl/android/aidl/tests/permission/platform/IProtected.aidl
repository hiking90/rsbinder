package android.aidl.tests.permission.platform;

import android.content.AttributionSource;

interface IProtected {
    @EnforcePermission(allOf={"INTERNET", "VIBRATE"})
    void ProtectedWithSourceAttribution(in AttributionSource source);
}
