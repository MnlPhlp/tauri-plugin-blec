import android.annotation.SuppressLint
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattService
import android.os.Build
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import com.plugin.blec.BleClientPlugin
import org.json.JSONArray
import java.util.UUID

private fun bytesToJson(bytes: ByteArray):JSONArray{
    val array = JSONArray()
    for (byte in bytes){
        array.put((byte))
    }
    return array
}

class Peripheral(private val activity: Activity, private val device: BluetoothDevice) {
    private var connected = false
    private var gatt: BluetoothGatt? = null
    private var services: List<BluetoothGattService> = listOf()
    private val characteristics: MutableMap<UUID,BluetoothGattCharacteristic> = mutableMapOf()
    private var onConnectionStateChange: ((connected:Boolean,error:String)->Unit)? = null
    private var onServicesDiscovered: ((connected:Boolean,error:String)->Unit)? = null
    private var notifyChannel:Channel? = null
    private val onReadInvoke:MutableMap<UUID,Invoke> = mutableMapOf()
    private val onWriteInvoke:MutableMap<UUID,Invoke> = mutableMapOf()
    private val callback = object:BluetoothGattCallback(){
        override fun onConnectionStateChange(gatt: BluetoothGatt?, status: Int, newState: Int) {
            if(status == BluetoothGatt.GATT_SUCCESS && newState==BluetoothGatt.STATE_CONNECTED && gatt!=null){
                this@Peripheral.connected = true
                this@Peripheral.gatt = gatt
                this@Peripheral.onConnectionStateChange?.invoke(true,"")
            } else {
                this@Peripheral.connected = false
                this@Peripheral.gatt = null
                this@Peripheral.onConnectionStateChange?.invoke(false,"Not connected. Status: $status, State: $newState")
            }
        }
        override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
            if (status != BluetoothGatt.GATT_SUCCESS) {
                this@Peripheral.services = listOf()
                this@Peripheral.onServicesDiscovered?.invoke(false,"No services discovered. Status $status")
            } else {
                this@Peripheral.services = gatt.services
                for (s in gatt.services){
                    for (c in s.characteristics){
                        this@Peripheral.characteristics[c.uuid] = c
                    }
                }
                this@Peripheral.onServicesDiscovered?.invoke(true,"")
            }
        }
        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray
        ) {
            if (this@Peripheral.notifyChannel == null){
                return
            }
            val notification = JSObject();
            notification.put("uuid",characteristic.uuid)
            notification.put("data",bytesToJson(value))
            this@Peripheral.notifyChannel!!.send(notification)
        }

        override fun onCharacteristicWrite(
            gatt: BluetoothGatt?,
            characteristic: BluetoothGattCharacteristic?,
            status: Int
        ) {
            val id = characteristic?.uuid ?: return
            val invoke = this@Peripheral.onWriteInvoke[id]!!
            if (status != BluetoothGatt.GATT_SUCCESS){
                invoke.reject("Write to characteristic $id failed with status $status")
            } else {
                invoke.resolve()
            }
            this@Peripheral.onWriteInvoke.remove(id)
        }

        override fun onCharacteristicRead(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
            status: Int
        ) {
            val id = characteristic?.uuid ?: return
            val invoke = this@Peripheral.onReadInvoke[id]!!
            if (status != BluetoothGatt.GATT_SUCCESS){
                invoke.reject("Read from characteristic $id failed with status $status")
            } else {
                val res = JSObject()
                res.put("value",bytesToJson(value))
                invoke.resolve(res)
            }
            this@Peripheral.onReadInvoke.remove(id)
        }
    }

    @SuppressLint("MissingPermission")
    fun connect(invoke:Invoke) {
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
        if (this.gatt == null){
            invoke.reject("No gatt server connected")
            return
        }
        this.onServicesDiscovered={ success, error ->
            if (success) {
                invoke.resolve()
            } else {
                invoke.reject(error)
            }
            this@Peripheral.onServicesDiscovered = null

        }
        this.gatt!!.discoverServices()
    }

    fun isConnected():Boolean {
        return this.connected
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
                    0,
                    charac.descriptors.map { desc ->  desc.uuid.toString()},
                ))
            }
            services.put(ResService(
                service.uuid.toString(),
                service.type == BluetoothGattService.SERVICE_TYPE_PRIMARY,
                characs
            ).toJson())
        }
        var res = JSObject()
        res.put("result",services)
        invoke.resolve(res)
    }

    class Notification(
        uuid: UUID,
        data: Array<Byte>
    )
    fun setNotifyChannel(channel: Channel){
        this.notifyChannel = channel;
    }

    @SuppressLint("MissingPermission")
    fun write(invoke: Invoke){
        val args = invoke.parseArgs(BleClientPlugin.WriteParams::class.java)
        if (this.gatt == null){
            invoke.reject("No gatt server connected")
            return
        }
        val charac = this.characteristics[args.characteristic!!]
        if (charac == null){
            invoke.reject("Characterisitc ${args.characteristic} not found")
            return
        }
        if (this.onWriteInvoke[args.characteristic] !=  null){
            this.onWriteInvoke[args.characteristic]!!.reject("write was overwritten before finishing")
        }
        this.onWriteInvoke[args.characteristic] = invoke
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            this.gatt!!.writeCharacteristic(charac,args.data!!,if (args.withResponse){BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT}else{BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE})
        } else {
            @Suppress("DEPRECATION")
            charac.value = args.data
            @Suppress("DEPRECATION")
            this.gatt!!.writeCharacteristic(charac)
        }
    }

    @SuppressLint("MissingPermission")
    fun read(invoke: Invoke){
        val args = invoke.parseArgs(BleClientPlugin.ReadParams::class.java)
        synchronized(this.onReadInvoke) {
            if (this.onReadInvoke[args.characteristic!!] != null) {
                this.onReadInvoke[args.characteristic]!!.reject("read was overwritten before finishing")
            }
            this.onReadInvoke[args.characteristic] = invoke
        }
        if (this.gatt == null){
            invoke.reject("No gatt server connected")
            return
        }
        val charac = this.characteristics[args.characteristic!!]
        if (charac == null){
            invoke.reject("Characterisitc ${args.characteristic} not found")
            return
        }
        this.gatt!!.readCharacteristic(charac)
    }
}