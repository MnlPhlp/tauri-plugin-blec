package com.plugin.blec

import android.annotation.SuppressLint
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanFilter.Builder
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanResult.TX_POWER_NOT_PRESENT
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.ParcelUuid
import android.util.SparseArray
import org.json.JSONArray
import org.json.JSONObject
import java.util.Base64

class BleDevice(
    val address: String,
    private val name: String,
    private val rssi: Int,
    private val connected: Boolean,
    private val bonded: Boolean,
    private val manufacturerData: SparseArray<ByteArray>?,
    private val serviceData: Map<ParcelUuid, ByteArray>?,
    private val services: List<ParcelUuid>?,
    private val txPowerLevel: Int?
){
    private val base64Encoder: Base64.Encoder = Base64.getEncoder()

    fun toJsObject():JSONObject{
        val obj = JSONObject()
        obj.put("address",address)
        obj.put("id",address)
        obj.put("name",name)
        obj.put("connected",connected)
        obj.put("bonded",bonded)
        obj.put("rssi",rssi)
        obj.put("txPowerLevel",txPowerLevel)
        // create Json Array from services
        val services = if (services != null) {
            val arr = JSONArray()
            for (service in services){
                arr.put(service)
            }
            arr
        } else { null }
        obj.put("services",services)
        // crate object from sparse Array
        val manufacturerData = if (manufacturerData != null) {
            val subObj = JSONObject()
            for (i in 0 until manufacturerData.size()) {
                val key = manufacturerData.keyAt(i)
                // get the object by the key.
                val value = manufacturerData.get(key)
                subObj.put(key.toString(),base64Encoder.encodeToString(value))
            }
            subObj
        } else { null }
        obj.put("manufacturerData",manufacturerData)
        // crate object from serviceData
        val serviceData = if (serviceData != null) {
            val subObj = JSONObject()
            for ((key, value) in serviceData){
                subObj.put(key.toString(),base64Encoder.encodeToString(value))
            }
            subObj
        } else { null }
        obj.put("serviceData",serviceData)
        return obj
    }
}

class BleClient(private val context: Context, private val plugin: BlecPlugin) {
    private var scanner: BluetoothLeScanner? = null
    private var manager: BluetoothManager? = null
    private var scanCb: ScanCallback? = null

    class ScanParams(
        val services: List<String>,
        val onDevice: Channel,
        val allowIbeacons: Boolean,
    ) {
        companion object {
            fun from(args: JSONObject): ScanParams {
                val services = args.optJSONArray("services")
                return ScanParams(
                    List(services?.length() ?: 0) { services!!.getString(it) },
                    Channel.from(args, "onDevice"),
                    args.optBoolean("allowIbeacons", false),
                )
            }
        }
    }
    @SuppressLint("MissingPermission")
    fun startScan(invoke: Invoke) {
        // check if running
        if (scanCb != null){
            invoke.reject("Scan already running")
            return
        }
        val args = ScanParams.from(invoke.args)

        // get scanner
        if (scanner == null) {
            manager = context.getSystemService(BluetoothManager::class.java)
                ?: throw RuntimeException("No bluetooth manager found")
            val bluetoothAdapter: BluetoothAdapter = manager!!.adapter
                ?: throw RuntimeException("No bluetooth adapter available")
            // check if bluetooth is on
            if (!bluetoothAdapter.isEnabled ) {
                val enableBtIntent = Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE)
                val activity = currentActivity
                if (activity != null) {
                    activity.startActivity(enableBtIntent)
                } else {
                    // Without an activity the intent needs its own task.
                    context.startActivity(enableBtIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                }
            }
            scanner = bluetoothAdapter.bluetoothLeScanner
                ?: throw RuntimeException("No bluetooth scanner available for adapter")
        }

        // clear old devices
        this.plugin.devices.clear()

        var filters: ArrayList<ScanFilter?>? = null
        if (args.services.isNotEmpty()) {
            filters = ArrayList()
            for (uuid in args.services) {
                filters.add(Builder().setServiceUuid(ParcelUuid.fromString(uuid)).build())
            }
        }
        val settings = ScanSettings.Builder()
            .setCallbackType(ScanSettings.CALLBACK_TYPE_ALL_MATCHES)
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .setLegacy(false)
            .build()

        scanCb = object: ScanCallback(){
            private fun sendResult(result: ScanResult){
                var name = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    result.device.alias
                } else {
                    result.device.name
                }
                if (name==null){
                    name = result.scanRecord?.deviceName
                }
                if (name == null) {
                    name = ""
                }
                val connected = this@BleClient.manager!!.getConnectionState(result.device,BluetoothProfile.GATT_SERVER) == BluetoothProfile.STATE_CONNECTED
                val bonded = result.device.getBondState() == BluetoothDevice.BOND_BONDED
                val txPower = if (result.txPower == TX_POWER_NOT_PRESENT) {
                    null
                } else {
                    result.txPower
                }
                val device = BleDevice(
                    result.device.address,
                    name,
                    result.rssi,
                    connected,
                    bonded,
                    result.scanRecord?.manufacturerSpecificData,
                    result.scanRecord?.serviceData,
                    result.scanRecord?.serviceUuids,
                    txPower
                )
                // Keep the Peripheral of a connected device: a fresh one would
                // not know about the open BluetoothGatt and leak it.
                val existing = this@BleClient.plugin.connected_devices[device.address]
                this@BleClient.plugin.devices[device.address] = existing
                    ?: Peripheral(this@BleClient.context, result.device, this@BleClient.plugin)
                val res = JSONObject()
                res.put("result", device.toJsObject())
                args.onDevice.send(res)
            }
            override fun onBatchScanResults(results: List<ScanResult>){
                for(result in results){
                    sendResult(result)
                }
            }
            override fun onScanFailed(errorCode: Int){
                println("Scan failed with error code $errorCode")
            }
            override fun onScanResult(callbackType: Int, result: ScanResult){
                sendResult(result)
            }
        }
        scanner?.startScan(filters, settings, scanCb!!)
        invoke.resolve()
    }

    @SuppressLint("MissingPermission")
    fun stopScan(invoke: Invoke){
        if (scanCb!=null) {
            scanner?.stopScan(scanCb!!)
            scanCb = null
        }
        invoke.resolve()
    }

    /**
     * Puts a known address back into [BleClientPlugin.devices] without scanning
     * for it, so a device stays connectable after a new scan cleared the map.
     * `getRemoteDevice` answers for any well formed address, so this says
     * nothing about the device being in range - connecting is what finds out.
     *
     * A Peripheral that is still around is reused: a fresh one would not know
     * about an open BluetoothGatt and leak it.
     */
    @SuppressLint("MissingPermission")
    fun retrievePeripheral(invoke: Invoke){
        val args = ConnectParams.from(invoke.args)
        if (manager == null) {
            manager = context.getSystemService(BluetoothManager::class.java)
        }
        val adapter = manager?.adapter
        if (adapter == null) {
            invoke.reject("retrieve_peripheral: no bluetooth adapter available")
            return
        }
        val remote = try {
            adapter.getRemoteDevice(args.address)
        } catch (e: IllegalArgumentException) {
            invoke.reject("retrieve_peripheral: '${args.address}' is not a bluetooth address")
            return
        }
        val existing = this.plugin.connected_devices[args.address]
            ?: this.plugin.devices[args.address]
        this.plugin.devices[args.address] = existing
            ?: Peripheral(this.context, remote, this.plugin)

        var name = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            remote.alias
        } else {
            remote.name
        }
        if (name == null) {
            name = ""
        }
        val connected = manager!!.getConnectionState(remote, BluetoothProfile.GATT_SERVER) ==
            BluetoothProfile.STATE_CONNECTED
        val bonded = remote.getBondState() == BluetoothDevice.BOND_BONDED
        // No advertisement was received, so there is no rssi, manufacturer or
        // service data to report.
        val device = BleDevice(remote.address, name, 0, connected, bonded, null, null, null, null)
        val res = JSONObject()
        res.put("result", device.toJsObject())
        invoke.resolve(res)
    }

    fun adapterState(invoke: Invoke) {
        val response = JSONObject()
        manager = context.getSystemService(BluetoothManager::class.java)
        if (manager == null){
            response.put("result","unknown")
        } else {
            val adapter = manager?.adapter
            if (adapter == null){
                response.put("result","unknown")
            } else {
                // check if bluetooth is on
                if (adapter.isEnabled ) {
                    response.put("result","on")
                } else {
                    response.put("result","off")
                }
            }
        }

        invoke.resolve(response)
        return
    }
}
