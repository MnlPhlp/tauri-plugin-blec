# Merged into the R8 configuration of every app that builds this module.
#
# The rust side of `blec` reaches these through JNI, by name, so R8 must
# neither remove nor rename them.
-keep class com.plugin.blec.Bridge { *; }
-keepclasseswithmembernames class * {
    native <methods>;
}

# The gatt and scan callbacks are only ever called by the framework.
-keep class * extends android.bluetooth.BluetoothGattCallback { *; }
-keep class * extends android.bluetooth.le.ScanCallback { *; }
