package com.plugin.blec

import Peripheral
import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanFilter.Builder
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context.MODE_PRIVATE
import android.content.Intent
import android.content.SharedPreferences
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.ParcelUuid
import android.provider.Settings
import android.util.SparseArray
import android.widget.Toast
import androidx.core.app.ActivityCompat
import androidx.core.app.ActivityCompat.startActivityForResult
import androidx.core.content.ContextCompat.getSystemService
import app.tauri.annotation.InvokeArg
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import java.util.Base64

class BleDevice(
    val address: String,
    private val name: String,
    private val rssi: Int,
    private val connected: Boolean,
    private val bonded: Boolean,
    private val manufacturerData: SparseArray<ByteArray>?,
    private val serviceData: Map<ParcelUuid, ByteArray>?,
    private val services: List<ParcelUuid>?
){
    private val base64Encoder: Base64.Encoder = Base64.getEncoder()

    fun toJsObject():JSObject{
        val obj = JSObject()
        obj.put("address",address)
        obj.put("id",address)
        obj.put("name",name)
        obj.put("connected",connected)
        obj.put("bonded",bonded)
        obj.put("rssi",rssi)
        // create Json Array from services
        val services = if (services != null) {
            val arr = JSArray();
            for (service in services){
                arr.put(service)
            }
            arr
        } else { null }
        obj.put("services",services)
        // crate object from sparse Array
        val manufacturerData = if (manufacturerData != null) {
            val subObj = JSObject()
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
            val subObj = JSObject()
            for ((key, value) in serviceData){
                subObj.put(key.toString(),base64Encoder.encodeToString(value))
            }
            subObj
        } else { null }
        obj.put("serviceData",serviceData)
        return obj
    }
}

class BleClient(private val activity: Activity, private val plugin: BleClientPlugin) {
    private var scanner: BluetoothLeScanner? = null;
    private var manager: BluetoothManager? = null;
    private var scanCb: ScanCallback? = null;

    private fun markFirstPermissionRequest(perm: String) {
        val sharedPreference: SharedPreferences =
            activity.getSharedPreferences("PREFS_PERMISSION_FIRST_TIME_ASKING", MODE_PRIVATE)
        sharedPreference.edit().putBoolean(perm, false).apply()
    }

    private fun firstPermissionRequest(perm: String): Boolean {
        return activity.getSharedPreferences("PREFS_PERMISSION_FIRST_TIME_ASKING", MODE_PRIVATE)
            .getBoolean(perm, true)
    }

    public fun checkPermissions(allowIbeacons: Boolean, askIfDenied: Boolean): Boolean {

        var permissions =  if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            arrayOf(
                Manifest.permission.BLUETOOTH_SCAN,
                Manifest.permission.BLUETOOTH_CONNECT
            )
        } else {
            arrayOf(
                Manifest.permission.BLUETOOTH_ADMIN,
                Manifest.permission.BLUETOOTH,
            )
        };
        if (allowIbeacons) {
            permissions += Manifest.permission.ACCESS_FINE_LOCATION
        }
        for (perm in permissions){
            if (ActivityCompat.checkSelfPermission(
                    activity,
                    perm
                ) != PackageManager.PERMISSION_GRANTED
            ) {
                if (firstPermissionRequest(perm) || activity.shouldShowRequestPermissionRationale(perm)) {
                    // this will open the permission dialog
                    markFirstPermissionRequest(perm)
                    activity.requestPermissions(permissions, 1)
                    return false
                } else{
                    if (!askIfDenied) {
                        return false
                    }

                    // this will open settings which asks for permission
                    val intent = Intent(
                        Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                        Uri.parse("package:${activity.packageName}")
                    )
                    activity.startActivity(intent)
                    Toast.makeText(activity, "Allow Permission: $perm", Toast.LENGTH_SHORT).show()
                    return false
                }
            }
        }
        return true
    }

    @InvokeArg
    class ScanParams {
        val services: ArrayList<String> = ArrayList()
        val onDevice: Channel? = null
        val allowIbeacons: Boolean = false
    }
    @SuppressLint("MissingPermission")
    fun startScan(invoke: Invoke) {
        // check if running
        if (scanCb != null){
            invoke.reject("Scan already running")
            return
        }
        val args = invoke.parseArgs(ScanParams::class.java)
        // check permission
        if (!checkPermissions(args.allowIbeacons, false)){
            invoke.reject("Missing permissions");
            return
        }

        // get scanner
        if (scanner == null) {
            manager = getSystemService(activity, BluetoothManager::class.java)
                ?: throw RuntimeException("No bluetooth manager found")
            val bluetoothAdapter: BluetoothAdapter = manager!!.adapter
                ?: throw RuntimeException("No bluetooth adapter available")
            // check if bluetooth is on
            if (!bluetoothAdapter.isEnabled ) {
                val enableBtIntent = Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE)
                startActivityForResult(activity, enableBtIntent,0,null)
            }
            scanner = bluetoothAdapter.bluetoothLeScanner
                ?: throw RuntimeException("No bluetooth scanner available for adapter")
        }

        // clear old devices
        this.plugin.devices.clear()

        var filters: ArrayList<ScanFilter?>? = null
        if (args.services.size > 0) {
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
                };
                if (name==null){
                    name = result.scanRecord?.deviceName
                }
                if (name == null) {
                    name = ""
                }
                val connected = this@BleClient.manager!!.getConnectionState(result.device,BluetoothProfile.GATT_SERVER) == BluetoothProfile.STATE_CONNECTED
                val bonded = result.device.getBondState() == BluetoothDevice.BOND_BONDED
                val device = BleDevice(
                    result.device.address,
                    name,
                    result.rssi,
                    connected,
                    bonded,
                    result.scanRecord?.manufacturerSpecificData,
                    result.scanRecord?.serviceData,
                    result.scanRecord?.serviceUuids
                )
                this@BleClient.plugin.devices[device.address] = Peripheral(this@BleClient.activity, result.device, this@BleClient.plugin)
                val res = JSObject()
                res.put("result", device.toJsObject())
                args.onDevice!!.send(res)
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

    fun adapterState(invoke: Invoke) {
        val response = JSObject()
        manager = getSystemService(activity, BluetoothManager::class.java)
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
