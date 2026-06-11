package com.plugin.blec


import Peripheral
import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.widget.Toast
import app.tauri.PermissionState
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.UUID


@InvokeArg
class ConnectParams{
    val address: String = ""
}

@TauriPlugin(
    permissions = [
        Permission(
            strings = [
                "android.permission.BLUETOOTH_SCAN",
                "android.permission.BLUETOOTH_CONNECT"
            ],
            alias = "bluetooth"
        ),
        Permission(
            strings = [
                "android.permission.BLUETOOTH_ADMIN",
                "android.permission.BLUETOOTH"
            ],
            alias = "bluetoothLegacy"
        ),
        Permission(
            strings = ["android.permission.ACCESS_FINE_LOCATION"],
            alias = "location"
        )
    ]
)
class BleClientPlugin(private val activity: Activity): Plugin(activity) {
    var devices: MutableMap<String, Peripheral> = mutableMapOf()
    var connected_devices: MutableMap<String, Peripheral> = mutableMapOf()
    var eventChannel: Channel? = null
    private val client = BleClient(activity, this)

    private var pendingAllowIbeacons = false
    private var pendingAskIfDenied = false
    
    @Command
    fun clear_devices(invoke: Invoke){
        this.devices.clear()
        invoke.resolve()
    }

    @Command
    fun start_scan(invoke: Invoke) {
        if (hasBTPermissions()) {
            client.startScan(invoke)
        } else {
            invoke.reject("start_scan: Missing bluetooth permission!")
        }
    }

    @Command
    fun stop_scan(invoke: Invoke){
        client.stopScan(invoke)
    }

    @Command
    fun events(invoke: Invoke){
        this.eventChannel = invoke.parseArgs(Channel::class.java)
        invoke.resolve()
    }

    @Command
    fun connect(invoke: Invoke){
        val args = invoke.parseArgs(ConnectParams::class.java)
        val device = this.devices[args.address]
        if (device == null){
            invoke.reject("connect: device '${args.address}' not found in discovered devices (known: ${this.devices.keys})")
            return
        }
        this.connected_devices[args.address] = device;
        device.connect(invoke)
    }

    @Command
    fun disconnect(invoke: Invoke){
        val args = invoke.parseArgs(ConnectParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("disconnect: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        this.connected_devices.remove(args.address)
        device.disconnect(invoke)
    }

    @Command
    fun is_connected(invoke: Invoke){
        val args = invoke.parseArgs(ConnectParams::class.java)
        val device = this.connected_devices[args.address]
        val res = JSObject()
        if (device == null){
            res.put("result", false)
        } else {
            res.put("result",device.isConnected())
        }
        invoke.resolve(res)
    }

    @Command
    fun is_bonded(invoke: Invoke){
        val args = invoke.parseArgs(ConnectParams::class.java)
        val device = this.devices[args.address]
        val res = JSObject()
        if (device == null){
            res.put("result", false)
        } else {
            res.put("result", device.isBonded())
        }
        invoke.resolve(res)
    }

    @Command
    fun discover_services(invoke:Invoke){
        val args = invoke.parseArgs(ConnectParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("discover_services: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.discoverServices(invoke)
    }

    @Command
    fun services(invoke:Invoke){
        val args = invoke.parseArgs(ConnectParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("services: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.services(invoke)
    }

    @InvokeArg
    class NotifyParams () {
        var address: String = ""
        var channel: Channel? = null
    }

    @Command
    fun notifications(invoke:Invoke){
        val args = invoke.parseArgs(NotifyParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("notifications: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.setNotifyChannel(args.channel!!)
        invoke.resolve()
    }

    @InvokeArg
    class WriteParams() {
        val address: String = ""
        val characteristic: UUID? = null
        val service: UUID? = null
        val data: ByteArray? = null
        val withResponse: Boolean = true
        
        // if a write times out, we will not write the data
        // a timeout of 0 means no timeout
        val timeout: Int = 0
        
        // if true, we will enqueue the write and immediately return;
        // the write is guaranteed to be sent, if the connection persists,
        // but the plugin won't know when it has happened
        val skipWaitingForWriteToComplete: Boolean = false
    }
    
    @Command
    fun write(invoke:Invoke){
        val args = invoke.parseArgs(WriteParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("write: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.write(invoke, args)
    }

    @InvokeArg
    class ReadParams(){
        val address: String = ""
        val characteristic: UUID? = null
        val service: UUID? = null
    }
    @Command
    fun read(invoke: Invoke){
        val args = invoke.parseArgs(ReadParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("read: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.read(invoke)
    }

    @Command
    fun subscribe(invoke: Invoke){
        val args = invoke.parseArgs(ReadParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("subscribe: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.subscribe(invoke,true)
    }

    @Command
    fun unsubscribe(invoke: Invoke){
        val args = invoke.parseArgs(ReadParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("unsubscribe: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.subscribe(invoke,false)
    }


    fun hasBTPermissions(): Boolean {
        val bluetoothAlias = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) "bluetooth" else "bluetoothLegacy"
        return getPermissionState(bluetoothAlias) == PermissionState.GRANTED
    }

    @InvokeArg
    class CheckPermissionsParams() {
        val allowIbeacons: Boolean = false
        val askIfDenied: Boolean = false
    }

    @Command
    fun check_permissions(invoke: Invoke) {
        val args = invoke.parseArgs(CheckPermissionsParams::class.java)
        pendingAllowIbeacons = args.allowIbeacons
        pendingAskIfDenied = args.askIfDenied

        val bluetoothAlias = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) "bluetooth" else "bluetoothLegacy"
        val neededAliases = mutableListOf(bluetoothAlias)
        if (args.allowIbeacons) neededAliases.add("location")

        val allGranted = neededAliases.all { getPermissionState(it) == PermissionState.GRANTED }
        if (allGranted) {
            val ret = JSObject()
            ret.put("result", true)
            invoke.resolve(ret)
            return
        }

        requestPermissionForAliases(neededAliases.toTypedArray(), invoke, "permissionsCallback")
    }

    @PermissionCallback
    fun permissionsCallback(invoke: Invoke) {
        val bluetoothAlias = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) "bluetooth" else "bluetoothLegacy"
        val neededAliases = mutableListOf(bluetoothAlias)
        if (pendingAllowIbeacons) neededAliases.add("location")

        val allGranted = neededAliases.all { getPermissionState(it) == PermissionState.GRANTED }

        if (!allGranted && pendingAskIfDenied) {
            val intent = Intent(
                Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                Uri.parse("package:${activity.packageName}")
            )
            activity.startActivity(intent)
            Toast.makeText(activity, "Please grant the 'Nearby devices' permission.", Toast.LENGTH_LONG).show()
        }

        val ret = JSObject()
        ret.put("result", allGranted)
        invoke.resolve(ret)
    }

    @InvokeArg
    class MtuParams(){
        val address: String = ""
        val mtu: Int = 517
    }
    @Command
    fun request_mtu(invoke: Invoke){
        val args = invoke.parseArgs(MtuParams::class.java)
        val device = this.connected_devices[args.address]
        if (device == null){
            invoke.reject("request_mtu: device '${args.address}' not in connected devices (connected: ${this.connected_devices.keys})")
            return
        }
        device.requestMtu(invoke, args.mtu)
    }

    @Command
    fun adapter_state(invoke: Invoke){
        client.adapterState(invoke);
    }
}
