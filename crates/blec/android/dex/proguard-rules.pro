# The rust side looks these up by name at runtime, so R8 must not touch them.
-keep class com.plugin.blec.Bridge { *; }
-keepclasseswithmembernames class * {
    native <methods>;
}

# Readable stack traces from the shrunk dex; the dex is small either way.
-dontobfuscate

# The gatt callbacks are only ever called by the framework.
-keep class * extends android.bluetooth.BluetoothGattCallback { *; }
-keep class * extends android.bluetooth.le.ScanCallback { *; }
