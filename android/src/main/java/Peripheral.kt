import android.annotation.SuppressLint
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import com.plugin.blec.BleClientPlugin
import org.json.JSONArray
import java.util.Base64
import java.util.UUID


class Peripheral(private val activity: Activity, private val device: BluetoothDevice, private val plugin: BleClientPlugin) {
    private val CLIENT_CHARACTERISTIC_CONFIGURATION_DESCRIPTOR: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
    private val base64Encoder: Base64.Encoder = Base64.getEncoder()

    private var connected = false
    private var bonded = false
    private var gatt: BluetoothGatt? = null
    private var services: List<BluetoothGattService> = listOf()
    private val characteristics: MutableMap<Pair<UUID,UUID>,BluetoothGattCharacteristic> = mutableMapOf()
    private var onConnectionStateChange: ((connected:Boolean,error:String)->Unit)? = null
    private var onServicesDiscovered: ((connected:Boolean,error:String)->Unit)? = null
    private var notifyChannel:Channel? = null
    private val onReadInvoke:MutableMap<Pair<UUID,UUID>,Invoke> = mutableMapOf()
    private val onWriteInvoke:MutableMap<Pair<UUID,UUID>,Invoke> = mutableMapOf()
    private var onDescriptorInvoke: Invoke? = null
    private var onMtuInvoke: Invoke? = null
    private var currentMtu = 517;

    private enum class Event{
        DeviceConnected,
        DeviceDisconnected
    }
    private fun sendEvent(event: Event){
        val channel = this.plugin.eventChannel?: return
        val data = JSObject()
        if (event == Event.DeviceConnected){
            data.put("DeviceConnected",this.device.address)
        } else if (event == Event.DeviceDisconnected){
            data.put("DeviceDisconnected",this.device.address)
        }
        println("sending event $data")
        channel.send(data)
    }

    private val callback = object:BluetoothGattCallback() {
        @SuppressLint("MissingPermission")
        override fun onConnectionStateChange(gatt: BluetoothGatt?, status: Int, newState: Int) {
            if (status == BluetoothGatt.GATT_SUCCESS && newState == BluetoothGatt.STATE_CONNECTED && gatt != null) {
                // gatt.requestMtu(517)
                this@Peripheral.connected = true
                this@Peripheral.gatt = gatt
                this@Peripheral.onConnectionStateChange?.invoke(true, "")
                this@Peripheral.sendEvent(Event.DeviceConnected)

            } else {
                this@Peripheral.connected = false
                this@Peripheral.gatt = null
                this@Peripheral.onConnectionStateChange?.invoke(
                    false,
                    "Not connected. Status: $status, State: $newState"
                )
                this@Peripheral.sendEvent(Event.DeviceDisconnected)
            }
        }

        override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
            println("onServicesDiscovered status $status, services ${gatt.services}")

            if (status != BluetoothGatt.GATT_SUCCESS) {
                this@Peripheral.services = listOf()
                this@Peripheral.onServicesDiscovered?.invoke(
                    false,
                    "No services discovered. Status $status"
                )
            } else {
                this@Peripheral.services = gatt.services
                for (s in gatt.services) {
                    for (c in s.characteristics) {
                        this@Peripheral.characteristics[Pair(c.uuid,c.service.uuid)] = c
                    }
                }
                this@Peripheral.onServicesDiscovered?.invoke(true, "")
            }
        }

        // Android 13 and upper
        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray
        ) {
            handleCharacteristicChanged(characteristic, value)
        }

        // Android 12 and below
        @Suppress("OVERRIDE_DEPRECATION")
        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic
        ) {
            val value = characteristic.value
            if (value != null) {
                handleCharacteristicChanged(characteristic, value)
            } else {
                Log.e("Peripheral", "Value received onCharacteristicChanged is null")
            }
        }

        // Extract the common logic into a helper function
        private fun handleCharacteristicChanged(
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray
        ) {
            this@Peripheral.notifyChannel?.let {
                synchronized(it) {
                    val notification = JSObject();
                    notification.put("uuid", characteristic.uuid)
                    notification.put("serviceUuid", characteristic.service.uuid)
                    notification.put("data", base64Encoder.encodeToString(value))
                    it.send(notification)
                }
            }
        }

        override fun onCharacteristicWrite(
            gatt: BluetoothGatt?,
            characteristic: BluetoothGattCharacteristic?,
            status: Int
        ) {
            val charac = characteristic ?: return
            val key = Pair(charac.uuid, charac.service.uuid)
            synchronized(this@Peripheral.onWriteInvoke) {
                val invoke = this@Peripheral.onWriteInvoke[key]
                if (invoke == null) {
                    Log.e("Peripheral", "Did not find tauri invoke obj for write on $key")
                } else {
                    if (status != BluetoothGatt.GATT_SUCCESS) {
                        invoke.reject("Write to characteristic ${charac.uuid} failed with status $status")
                    } else {
                        invoke.resolve()
                    }
                }
                this@Peripheral.onWriteInvoke.remove(key)
            }
        }

        override fun onCharacteristicRead(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
            status: Int
        ) {
            val key = Pair(characteristic.uuid, characteristic.service.uuid)
            synchronized(this@Peripheral.onReadInvoke) {
                val invoke = this@Peripheral.onReadInvoke[key]
                if (invoke == null) {
                    Log.e("Peripheral", "Did not find tauri invoke obj for read on $key")
                } else {
                    if (status != BluetoothGatt.GATT_SUCCESS) {
                        invoke.reject("Read from characteristic ${characteristic.uuid} failed with status $status")
                    } else {
                        val res = JSObject()
                        res.put("value", base64Encoder.encodeToString(value))
                        invoke.resolve(res)
                    }
                }
                this@Peripheral.onReadInvoke.remove(key)
            }
        }

        override fun onDescriptorWrite(
            gatt: BluetoothGatt?,
            descriptor: BluetoothGattDescriptor?,
            status: Int
        ) {
            val invoke = this@Peripheral.onDescriptorInvoke
            this@Peripheral.onDescriptorInvoke = null
            if (invoke == null) {
                Log.e("Peripheral", "Did not find tauri invoke obj for descriptor write")
                return
            }
            if (status != BluetoothGatt.GATT_SUCCESS) {
                invoke.reject("descriptor write failed with status: $status")
            } else if (descriptor?.uuid != CLIENT_CHARACTERISTIC_CONFIGURATION_DESCRIPTOR) {
                invoke.reject("unexpected write to descriptor: ${descriptor?.uuid}")
            } else {
                invoke.resolve()
            }
        }

        override fun onMtuChanged(gatt: BluetoothGatt?, mtu: Int, status: Int) {
            println("MTU changed to $mtu with status $status")
            currentMtu = mtu
            val invoke = this@Peripheral.onMtuInvoke
            this@Peripheral.onMtuInvoke = null
            if (status != BluetoothGatt.GATT_SUCCESS) {
                invoke?.reject("mtu change failed: $status")
            } else {
                val res = JSObject()
                res.put("mtu", mtu)
                invoke?.resolve(res)
            }
        }
    }

    @SuppressLint("MissingPermission")
    fun connect(invoke:Invoke) {
        println("connect android implementation called")
        this.onConnectionStateChange = { success, error ->
            if(success){
                invoke.resolve()
            } else {
                invoke.reject(error)
            }
            this@Peripheral.onConnectionStateChange = null
        }
        this.device.connectGatt(activity, false, this.callback)
    }

    @SuppressLint("MissingPermission")
    fun discoverServices(invoke:Invoke){
        val gatt = this.gatt
        if (gatt == null){
            invoke.reject("No gatt server connected")
            return
        }
        this.onServicesDiscovered={ success, error ->
            if (success) {
                invoke.resolve()
            } else {
                invoke.reject(error)
            }
            this.onServicesDiscovered = null

        }

        Handler(Looper.getMainLooper()).post(Runnable {
            if (!gatt.discoverServices()) {
                invoke.reject("failed to start service discovery");
            }
            println("service discovery started")
        })
    }

    fun isConnected():Boolean {
        return this.connected
    }

    @SuppressLint("MissingPermission")
    fun isBonded(): Boolean {
        return this.device.bondState == BluetoothDevice.BOND_BONDED
    }

    @SuppressLint("MissingPermission")
    fun disconnect(invoke: Invoke){
        this.gatt?.disconnect()
        this.connected = false
        invoke.resolve()
    }

     class ResCharacteristic (
         private val uuid: String,
         private val properties: Int,
         private val descriptors: List<String>
     ){
         fun toJson():JSObject{
             val ret = JSObject()
             ret.put("uuid",uuid)
             ret.put("properties",properties)
             val descriptors = JSONArray()
             for (desc in this.descriptors){
                 descriptors.put(desc)
             }
             ret.put("descriptors",descriptors)
            return ret
         }
     }

    class ResService (
        private val uuid: String,
        private val primary: Boolean,
        private val characs: List<ResCharacteristic>,
    ){
        fun toJson():JSObject{
            val ret = JSObject()
            ret.put("uuid",uuid)
            ret.put("primary",primary)
            val characs = JSONArray()
            for (char in this.characs){
                characs.put(char.toJson())
            }
            ret.put("characs",characs)
            return ret
        }
    }

    fun services(invoke:Invoke){
        val services = JSONArray()
        for(service in this.services){
            val characs:MutableList<ResCharacteristic> = mutableListOf()
            for (charac in service.characteristics){
                characs.add(ResCharacteristic(
                    charac.uuid.toString(),
                    charac.properties,
                    charac.descriptors.map { desc ->  desc.uuid.toString()},
                ))
            }
            services.put(ResService(
                service.uuid.toString(),
                service.type == BluetoothGattService.SERVICE_TYPE_PRIMARY,
                characs
            ).toJson())
        }
        val res = JSObject()
        res.put("result",services)
        invoke.resolve(res)
    }

    fun setNotifyChannel(channel: Channel){
        this.notifyChannel = channel;
    }

    @SuppressLint("MissingPermission")
    fun write(invoke: Invoke){
        val args = invoke.parseArgs(BleClientPlugin.WriteParams::class.java)
        val gatt = this.gatt;
        if (gatt == null){
            invoke.reject("No gatt server connected")
            return
        }
        val key = Pair(args.characteristic!!, args.service!!)
        val charac = this.characteristics[key]
        if (charac == null){
            invoke.reject("Characterisitc ${args.characteristic} not found")
            return
        }
        synchronized(this.onWriteInvoke) {
            if (this.onWriteInvoke[key] != null) {
                this.onWriteInvoke[key]!!.reject("write was overwritten before finishing")
            }
            this.onWriteInvoke[key] = invoke
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            gatt.writeCharacteristic(charac,args.data!!,if (args.withResponse){BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT}else{BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE})
        } else {
            @Suppress("DEPRECATION")
            charac.value = args.data
            @Suppress("DEPRECATION")
            gatt.writeCharacteristic(charac)
        }
    }

    @SuppressLint("MissingPermission")
    fun read(invoke: Invoke){
        val args = invoke.parseArgs(BleClientPlugin.ReadParams::class.java)
        val gatt = this.gatt;
        if (gatt == null){
            invoke.reject("No gatt server connected")
            return
        }
        val key = Pair(args.characteristic!!, args.service!!)
        val charac = this.characteristics[key]
        if (charac == null){
            invoke.reject("Characteristic ${args.characteristic} not found")
            return
        }
        synchronized(this.onReadInvoke) {
            if (this.onReadInvoke[key] != null) {
                this.onReadInvoke[key]!!.reject("read was overwritten before finishing")
            }
            this.onReadInvoke[key] = invoke
        }
        gatt.readCharacteristic(charac)
    }

    @SuppressLint("MissingPermission")
    fun subscribe(invoke: Invoke,enabled: Boolean){
        val args = invoke.parseArgs(BleClientPlugin.ReadParams::class.java)
        val gatt = this.gatt;
        if (gatt == null){
            invoke.reject("No gatt server connected")
            return
        }
        val charac = this.characteristics[Pair(args.characteristic!!,args.service!!)]
        if (charac == null){
            invoke.reject("Characteristic ${args.characteristic} not found")
            return
        }

        if (!gatt.setCharacteristicNotification(charac,enabled)){
            invoke.reject("Failed to set notification status")
            return
        }
        val descriptor: BluetoothGattDescriptor? = charac.getDescriptor(CLIENT_CHARACTERISTIC_CONFIGURATION_DESCRIPTOR)
        if (descriptor == null) {
            invoke.reject("CCCD descriptor not found on characteristic ${args.characteristic}")
            return
        }
        if (this.onDescriptorInvoke != null) {
            this.onDescriptorInvoke!!.reject("descriptor write was overwritten before finishing")
        }
        this.onDescriptorInvoke = invoke
        val data = if (enabled){BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE}else{BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE}
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            gatt.writeDescriptor(descriptor,data)
        } else {
            @Suppress("DEPRECATION")
            descriptor.value = data
            @Suppress("DEPRECATION")
            gatt.writeDescriptor(descriptor)
        }
    }

    @SuppressLint("MissingPermission")
    fun requestMtu(invoke: Invoke, mtu: Int){
        val gatt = this.gatt
        if (gatt == null) {
            invoke.reject("No gatt server connected")
            return
        }
        if (this.onMtuInvoke != null) {
            this.onMtuInvoke!!.reject("mtu request was overwritten before finishing")
        }
        onMtuInvoke = invoke
        Handler(Looper.getMainLooper()).post(Runnable {
            if (!gatt.requestMtu(mtu)) {
                this@Peripheral.onMtuInvoke = null
                invoke.reject("Failed to request mtu")
            }
        })
    }
}
