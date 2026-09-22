package com.plugin.blec

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import org.json.JSONObject

/** Arbitrary; nobody owns the Activity, so the result is never delivered to us. */
private const val PERMISSION_REQUEST_CODE = 0xB1EC

private fun result(value: Any): JSONObject = JSONObject().put("result", value)

/**
 * The commands [invoke] dispatches to, and the state they share.
 *
 * Instantiated once by [init]; everything it needs from android is a
 * `Context`, except requesting permissions, which needs an Activity and
 * therefore goes through [currentActivity].
 */
class BlecPlugin(private val context: Context) {
    /** The peripherals the last scan reported, by address. */
    var devices: MutableMap<String, Peripheral> = mutableMapOf()

    /** The peripherals with a `BluetoothGatt`, which must not be replaced. */
    var connected_devices: MutableMap<String, Peripheral> = mutableMapOf()

    var eventChannel: Channel? = null

    private val client = BleClient(context, this)

    private var pendingPermissions: PendingPermissions? = null

    private class PendingPermissions(
        val invoke: Invoke,
        val permissions: List<String>,
        val askIfDenied: Boolean,
    ) {
        /**
         * Whether the activity was paused since the request was made, which is
         * what the permission dialog taking the foreground looks like from
         * here. See [onActivityResumed].
         */
        var wasPaused = false
    }

    fun handle(cmd: String, invoke: Invoke) {
        when (cmd) {
            "clear_peripherals" -> clearPeripherals(invoke)
            "start_scan" -> startScan(invoke)
            "stop_scan" -> client.stopScan(invoke)
            "retrieve_peripheral" -> retrievePeripheral(invoke)
            "events" -> events(invoke)
            "adapter_state" -> client.adapterState(invoke)
            "check_permissions" -> checkPermissions(invoke)
            "connect" -> connect(invoke)
            "disconnect" -> disconnect(invoke)
            "is_connected" -> isConnected(invoke)
            "is_bonded" -> isBonded(invoke)
            "discover_services" -> withConnected(cmd, invoke) { it.discoverServices(invoke) }
            "services" -> withConnected(cmd, invoke) { it.services(invoke) }
            "notifications" -> notifications(invoke)
            "write" -> write(invoke)
            "read" -> withConnected(cmd, invoke) { it.read(invoke) }
            "subscribe" -> withConnected(cmd, invoke) { it.subscribe(invoke, true) }
            "unsubscribe" -> withConnected(cmd, invoke) { it.subscribe(invoke, false) }
            "request_mtu" -> requestMtu(invoke)
            else -> invoke.reject("unknown command '$cmd'")
        }
    }

    /**
     * Runs [block] on the peripheral the command addresses, or rejects when
     * nothing is connected under that address.
     */
    private fun withConnected(cmd: String, invoke: Invoke, block: (Peripheral) -> Unit) {
        val address = ConnectParams.from(invoke.args).address
        val device = this.connected_devices[address]
        if (device == null) {
            invoke.reject(
                "$cmd: device '$address' not in connected devices " +
                    "(connected: ${this.connected_devices.keys})"
            )
            return
        }
        block(device)
    }

    private fun clearPeripherals(invoke: Invoke) {
        this.devices.clear()
        invoke.resolve()
    }

    private fun startScan(invoke: Invoke) {
        if (hasBTPermissions()) {
            client.startScan(invoke)
        } else {
            invoke.reject("start_scan: Missing bluetooth permission!")
        }
    }

    private fun retrievePeripheral(invoke: Invoke) {
        if (hasBTPermissions()) {
            client.retrievePeripheral(invoke)
        } else {
            invoke.reject("retrieve_peripheral: Missing bluetooth permission!")
        }
    }

    private fun events(invoke: Invoke) {
        this.eventChannel = Channel.from(invoke.args, "channel")
        invoke.resolve()
    }

    private fun connect(invoke: Invoke) {
        val args = ConnectParams.from(invoke.args)
        // A Peripheral that still holds a BluetoothGatt must be reused: replacing
        // it with the scan-fresh object from `devices` would leak that gatt and
        // leave the device connected with nobody able to disconnect it.
        val existing = this.connected_devices[args.address]
        val device = if (existing != null && existing.hasGatt()) {
            existing
        } else {
            this.devices[args.address] ?: existing
        }
        if (device == null) {
            invoke.reject(
                "connect: device '${args.address}' not found in discovered devices " +
                    "(known: ${this.devices.keys})"
            )
            return
        }
        this.connected_devices[args.address] = device
        device.connect(invoke)
    }

    private fun disconnect(invoke: Invoke) {
        val args = ConnectParams.from(invoke.args)
        val device = this.connected_devices[args.address]
        if (device == null) {
            invoke.reject(
                "disconnect: device '${args.address}' not in connected devices " +
                    "(connected: ${this.connected_devices.keys})"
            )
            return
        }
        // Only forget the device once it is really disconnected — removing it
        // first makes a slow disconnect unreachable for any later
        // disconnect/is_connected call, which is how links end up half-open.
        device.disconnect(invoke) {
            this.connected_devices.remove(args.address)
        }
    }

    private fun isConnected(invoke: Invoke) {
        val args = ConnectParams.from(invoke.args)
        val device = this.connected_devices[args.address]
        invoke.resolve(result(device?.isConnected() ?: false))
    }

    private fun isBonded(invoke: Invoke) {
        val args = ConnectParams.from(invoke.args)
        val device = this.devices[args.address]
        invoke.resolve(result(device?.isBonded() ?: false))
    }

    private fun notifications(invoke: Invoke) {
        val args = NotifyParams.from(invoke.args)
        val device = this.connected_devices[args.address]
        if (device == null) {
            invoke.reject(
                "notifications: device '${args.address}' not in connected devices " +
                    "(connected: ${this.connected_devices.keys})"
            )
            return
        }
        device.setNotifyChannel(args.channel)
        invoke.resolve()
    }

    private fun write(invoke: Invoke) {
        val args = WriteParams.from(invoke.args)
        val device = this.connected_devices[args.address]
        if (device == null) {
            invoke.reject(
                "write: device '${args.address}' not in connected devices " +
                    "(connected: ${this.connected_devices.keys})"
            )
            return
        }
        device.write(invoke, args)
    }

    private fun requestMtu(invoke: Invoke) {
        val args = MtuParams.from(invoke.args)
        val device = this.connected_devices[args.address]
        if (device == null) {
            invoke.reject(
                "request_mtu: device '${args.address}' not in connected devices " +
                    "(connected: ${this.connected_devices.keys})"
            )
            return
        }
        device.requestMtu(invoke, args.mtu)
    }

    /**
     * The permissions a scan needs on this android version. iBeacons are
     * advertisements that can be used to derive a location, so android only
     * reports them with the location permission.
     */
    private fun requiredPermissions(allowIbeacons: Boolean): List<String> {
        val permissions = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            mutableListOf(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT)
        } else {
            mutableListOf(Manifest.permission.BLUETOOTH, Manifest.permission.BLUETOOTH_ADMIN)
        }
        if (allowIbeacons) {
            permissions.add(Manifest.permission.ACCESS_FINE_LOCATION)
        }
        return permissions
    }

    private fun missing(permissions: List<String>): List<String> = permissions.filter {
        context.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED
    }

    fun hasBTPermissions(): Boolean = missing(requiredPermissions(false)).isEmpty()

    /**
     * Answers whether the app may use BLE, asking the user if it may not.
     *
     * The answer only arrives once the user is done with the dialog, which
     * [onActivityResumed] notices.
     */
    private fun checkPermissions(invoke: Invoke) {
        val args = CheckPermissionsParams.from(invoke.args)
        val needed = requiredPermissions(args.allowIbeacons)
        val missing = missing(needed)
        if (missing.isEmpty()) {
            invoke.resolve(result(true))
            return
        }
        val activity = currentActivity
        if (activity == null) {
            invoke.reject(
                "check_permissions: no activity available to request permissions from; " +
                    "call blec::android::set_activity"
            )
            return
        }
        this.pendingPermissions?.invoke?.reject("check_permissions was superseded by a new request")
        this.pendingPermissions = PendingPermissions(invoke, needed, args.askIfDenied)
        activity.requestPermissions(missing.toTypedArray(), PERMISSION_REQUEST_CODE)
    }

    /** Notes that the permission dialog may have taken the foreground. */
    fun onActivityPaused() {
        this.pendingPermissions?.wasPaused = true
    }

    /**
     * Picks up the result of a permission request.
     *
     * Nobody here owns the Activity subclass, so `onRequestPermissionsResult`
     * is not available: re-checking the permissions once the activity is back
     * in the foreground answers the same question.
     *
     * Only a resume that follows a pause does, though. The dialog is an
     * activity of its own, so showing it pauses ours; a resume without a pause
     * in between means the dialog never ran, which is what an app that was in
     * the background when the request was made sees when the user returns to
     * it. Answering there would report the permissions as denied before the
     * user was ever asked. A resume that finds the permissions granted is
     * conclusive either way, and covers a device that somehow shows the dialog
     * without pausing us.
     */
    fun onActivityResumed() {
        val pending = this.pendingPermissions ?: return
        val granted = missing(pending.permissions).isEmpty()
        if (!pending.wasPaused && !granted) {
            return
        }
        this.pendingPermissions = null
        // Android shows the dialog only once. When the user denied it before,
        // the app settings page is the only way left to grant it.
        if (!granted && pending.askIfDenied) {
            val activity = currentActivity
            if (activity != null) {
                try {
                    activity.startActivity(
                        Intent(
                            Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                            Uri.parse("package:${activity.packageName}")
                        )
                    )
                    Toast.makeText(
                        activity,
                        "Please grant the 'Nearby devices' permission.",
                        Toast.LENGTH_LONG
                    ).show()
                } catch (e: Exception) {
                    Log.w(TAG, "could not open the app settings: ${e.message}")
                }
            }
        }
        pending.invoke.resolve(result(granted))
    }
}
