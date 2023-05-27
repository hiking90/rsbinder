package android.aidl.loggable;
import android.aidl.loggable.Data;

interface ILoggableInterface {
    @SuppressWarnings(value={"inout-parameter", "out-array", "out-nullable"})
    String[] LogThis(boolean boolValue, inout boolean[] boolArray, byte byteValue,
            inout byte[] byteArray, char charValue, inout char[] charArray, int intValue,
            inout int[] intArray, long longValue, inout long[] longArray, float floatValue,
            inout float[] floatArray, double doubleValue, inout double[] doubleArray,
            String stringValue, inout String[] stringArray, inout List<String> listValue,
            in Data dataValue, @nullable IBinder binderValue,
            inout @nullable ParcelFileDescriptor pfdValue,
            inout ParcelFileDescriptor[] pfdArray);

    interface ISub {
        void Log(int value);
    }
}
