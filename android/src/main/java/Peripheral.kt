import android.annotation.SuppressLint
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothStatusCodes
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import com.plugin.blec.BleClientPlugin
import org.json.JSONArray
import java.util.concurrent.atomic.AtomicInteger as AtomicInt
import java.util.ArrayDeque
import java.util.Base64
import java.util.UUID


class Peripheral(
    private val activity: Activity,
    private val device: BluetoothDevice,
    private val plugin: BleClientPlugin
) {
    // Retry state for connect/discover
    private var connectAttempts = 0
    private val maxConnectAttempts = 4
    private val connectRetryDelayMs = 350L
    private var discoverAttempts = 0
    private val maxDiscoverAttempts = 3
    private val discoverRetryDelayMs = 200L
    private val CLIENT_CHARACTERISTIC_CONFIGURATION_DESCRIPTOR: UUID =
        UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
    private val base64Encoder: Base64.Encoder = Base64.getEncoder()

    private var connected = false
    private var bonded = false
    private var gatt: BluetoothGatt? = null
    private var services: List<BluetoothGattService> = listOf()
    private val characteristics: MutableMap<Pair<UUID, UUID>, BluetoothGattCharacteristic> = mutableMapOf()
    private var onConnectionStateChange: ((connected: Boolean, error: String) -> Unit)? = null
    private var onServicesDiscovered: ((connected: Boolean, error: String) -> Unit)? = null
    private var notifyChannel: Channel? = null
    private val onReadInvoke: MutableMap<Pair<UUID, UUID>, ReadOp> = mutableMapOf()
    private val writeQueueLock = Any()
    private val writeQueue: ArrayDeque<PendingWrite> = ArrayDeque()

    private var activeWrite: PendingWrite? = null

    private var onDescriptorInvoke: DescriptorOp? = null
    private var onMtuInvoke: Invoke? = null
    private var currentMtu = 517;

    private val retryHandler = Handler(Looper.getMainLooper())

    // maxAttempts for writes; only counts actual failure, waiting on the BT-chip (WRITE_REQUEST_BUSY) does not count as an attempt
    private val maxAttempts = 100
    private val writeRetryDelayMs = 50L
    private val writeCallbackWaitNoResponseMs = 500L
    private val writeCallbackWaitWithResponseMs = 750L

    private val writeCount: AtomicInt = AtomicInt(0)

    private fun runOnMain(block: () -> Unit) {
        retryHandler.postDelayed(block, 1L)
    }

    private data class PendingWrite(
        val id: Int,
        val key: Pair<UUID, UUID>,
        val characteristic: BluetoothGattCharacteristic,
        var invoke: Invoke?,
        val data: ByteArray,
        val withResponse: Boolean,
        var attempt: Int = 0,
        val timeoutAfter: Long = 0L,
        var timeSentAt: Long = 0L,
    )

    private data class ReadOp(
        val invoke: Invoke,
        var attempt: Int = 1,
    )

    private data class DescriptorOp(
        val invoke: Invoke,
        val data: ByteArray,
        var attempt: Int = 1,
    )

    private enum class Event {
        DeviceConnected,
        DeviceDisconnected
    }

    private fun sendEvent(event: Event) {
        val channel = this.plugin.eventChannel ?: return
        val data = JSObject()
        if (event == Event.DeviceConnected) {
            data.put("DeviceConnected", this.device.address)
        } else if (event == Event.DeviceDisconnected) {
            data.put("DeviceDisconnected", this.device.address)
        }
        println("sending event $data")
        channel.send(data)
    }

    private val callback = object : BluetoothGattCallback() {
        @SuppressLint("MissingPermission")
        override fun onConnectionStateChange(gatt: BluetoothGatt?, status: Int, newState: Int) {
            if (status == BluetoothGatt.GATT_SUCCESS && newState == BluetoothGatt.STATE_CONNECTED && gatt != null) {
                this@Peripheral.connected = true
                this@Peripheral.gatt = gatt
                this@Peripheral.onConnectionStateChange?.invoke(true, "")
                this@Peripheral.sendEvent(Event.DeviceConnected)
            } else {
                // Either a connection failure (status != GATT_SUCCESS) or a
                // disconnection. In both cases the BluetoothGatt instance must
                // be closed to release the underlying client interface,
                // otherwise repeated connect/disconnect cycles run into the
                // 30 client limit and start failing with status 133.
                this@Peripheral.connected = false
                val existingGatt = this@Peripheral.gatt ?: gatt
                this@Peripheral.gatt = null
                try {
                    existingGatt?.close()
                } catch (e: Exception) {
                    Log.w("Peripheral", "Failed to close gatt: ${e.message}")
                }
                val error = if (status != BluetoothGatt.GATT_SUCCESS) {
                    "Connection failed. Status: $status (${statusCodeName(status)}), State: $newState"
                } else {
                    "Disconnected. State: $newState"
                }
                this@Peripheral.onConnectionStateChange?.invoke(false, error)
                this@Peripheral.sendEvent(Event.DeviceDisconnected)
            }
        }

        override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
            Log.d("Peripheral", "onServicesDiscovered status $status, services ${gatt.services}")
            if (status != BluetoothGatt.GATT_SUCCESS) {
                // Android BLE edge-case handling: status 133, 62, 129 are
                // commonly transient and can succeed on a second attempt.
                val isEdgeCase = (status == 133 || status == 62 || status == 129)
                if (isEdgeCase && discoverAttempts < maxDiscoverAttempts) {
                    discoverAttempts += 1
                    Log.w(
                        "Peripheral",
                        "Service discovery failed (status $status ${statusCodeName(status)}), retrying attempt $discoverAttempts/$maxDiscoverAttempts"
                    )
                    retryHandler.postDelayed({
                        if (this@Peripheral.connected) {
                            gatt.discoverServices()
                        } else {
                            this@Peripheral.onServicesDiscovered?.invoke(
                                false,
                                "Service discovery aborted: disconnected during retry"
                            )
                            discoverAttempts = 0
                        }
                    }, discoverRetryDelayMs)
                    return
                }
                discoverAttempts = 0
                this@Peripheral.services = listOf()
                this@Peripheral.onServicesDiscovered?.invoke(
                    false,
                    "No services discovered. Status $status (${statusCodeName(status)}) after ${discoverAttempts + 1} attempt(s)"
                )
            } else {
                discoverAttempts = 0
                this@Peripheral.services = gatt.services
                for (s in gatt.services) {
                    for (c in s.characteristics) {
                        this@Peripheral.characteristics[Pair(c.uuid, c.service.uuid)] = c
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
            @Suppress("DEPRECATION")
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

        @Suppress("OVERRIDE_DEPRECATION")
        override fun onCharacteristicWrite(
            gatt: BluetoothGatt?,
            characteristic: BluetoothGattCharacteristic?,
            status: Int
        ) {
            val nonNullGatt = gatt ?: return
            val nonNullCharacteristic = characteristic ?: return
            
            runOnMain {
                processOnCharWrite(nonNullGatt, nonNullCharacteristic, status)
            }
        }

        @Suppress("OVERRIDE_DEPRECATION")
        fun processOnCharWrite(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            status: Int
        ) {
            Log.v("Peripheral", "onCharacteristicWrite for ${characteristic.uuid} with status $status")
            val key = Pair(characteristic.uuid, characteristic.service.uuid)
            
            @Suppress("DEPRECATION")
            val value = characteristic.value ?: ByteArray(0)

            val current = this@Peripheral.activeWrite
            if (current == null || current.key != key) {
                Log.w("Peripheral", "Received stale write callback for $key, ignoring")
                return
            }

            var success = status == BluetoothGatt.GATT_SUCCESS
            if (success && current.withResponse) {
                success = value.contentEquals(current.data)
            }

            if (success) {
                this@Peripheral.activeWrite = null
                current.invoke?.resolve()
            } else {
                current.timeSentAt = 0L

                if (current.attempt < this@Peripheral.maxAttempts && !this@Peripheral.isWriteTimedOut(current)) {
                    current.attempt += 1
                    Log.w(
                        "Peripheral",
                        "Write on ${characteristic.uuid} failed (status $status, attempt ${current.attempt}/${this@Peripheral.maxAttempts})"
                    )
                } else {
                    this@Peripheral.activeWrite = null
                    current.invoke?.reject(
                        "Write to characteristic ${characteristic.uuid} failed after ${current.attempt} attempts with status $status (${
                            statusCodeName(
                                status
                            )
                        })"
                    )
                }
            }
            this@Peripheral.processWriteQueue()
        }

        override fun onCharacteristicRead(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
            status: Int
        ) {
            val key = Pair(characteristic.uuid, characteristic.service.uuid)
            val op = synchronized(this@Peripheral.onReadInvoke) {
                this@Peripheral.onReadInvoke.remove(key)
            }
            if (op == null) {
                Log.e("Peripheral", "Did not find tauri invoke obj for read on $key")
                return
            }
            if (status == BluetoothGatt.GATT_SUCCESS) {
                val res = JSObject()
                res.put("value", base64Encoder.encodeToString(value))
                op.invoke.resolve(res)
                return
            }
            if (op.attempt < this@Peripheral.maxAttempts) {
                val nextAttempt = op.attempt + 1
                Log.w(
                    "Peripheral",
                    "Read on ${characteristic.uuid} failed (status $status, attempt ${op.attempt}/${this@Peripheral.maxAttempts}), retrying"
                )
                this@Peripheral.retryHandler.postDelayed({
                    this@Peripheral.startRead(key, characteristic, op.copy(attempt = nextAttempt))
                }, this@Peripheral.writeRetryDelayMs)
            } else {
                op.invoke.reject(
                    "Read from characteristic ${characteristic.uuid} failed after ${op.attempt} attempts with status $status (${
                        statusCodeName(
                            status
                        )
                    })"
                )
            }
        }

        override fun onDescriptorWrite(
            gatt: BluetoothGatt?,
            descriptor: BluetoothGattDescriptor?,
            status: Int
        ) {
            val op = this@Peripheral.onDescriptorInvoke
            this@Peripheral.onDescriptorInvoke = null
            if (op == null) {
                Log.e("Peripheral", "Did not find tauri invoke obj for descriptor write")
                return
            }
            if (status == BluetoothGatt.GATT_SUCCESS) {
                if (descriptor?.uuid != CLIENT_CHARACTERISTIC_CONFIGURATION_DESCRIPTOR) {
                    op.invoke.reject("unexpected write to descriptor: ${descriptor?.uuid}")
                } else {
                    op.invoke.resolve()
                }
                return
            }
            val desc = descriptor
            if (op.attempt < this@Peripheral.maxAttempts && desc != null) {
                val nextAttempt = op.attempt + 1
                Log.w(
                    "Peripheral",
                    "Descriptor write failed (status $status, attempt ${op.attempt}/${this@Peripheral.maxAttempts}), retrying"
                )
                this@Peripheral.retryHandler.postDelayed({
                    this@Peripheral.startDescriptorWrite(desc, op.copy(attempt = nextAttempt))
                }, this@Peripheral.writeRetryDelayMs)
            } else {
                op.invoke.reject(
                    "descriptor write failed after ${op.attempt} attempts with status $status (${
                        statusCodeName(
                            status
                        )
                    })"
                )
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
    fun connect(invoke: Invoke) {
        println("connect android implementation called")
        connectAttempts = 0
        connectInternal(invoke)
    }

    @SuppressLint("MissingPermission")
    private fun connectInternal(invoke: Invoke) {
        this.onConnectionStateChange = { success, error ->
            if (success) {
                this@Peripheral.onConnectionStateChange = null
                invoke.resolve()
            } else {
                // Android BLE edge-case handling: status 133/62/129 are
                // commonly transient. Retry a few times before giving up.
                val isEdgeCase = error.contains("Status: 133") ||
                        error.contains("Status: 62") ||
                        error.contains("Status: 129")
                if (isEdgeCase && connectAttempts < maxConnectAttempts) {
                    connectAttempts += 1
                    Log.w(
                        "Peripheral",
                        "Connect failed ($error), retrying attempt $connectAttempts/$maxConnectAttempts"
                    )
                    retryHandler.postDelayed({
                        connectInternal(invoke)
                    }, connectRetryDelayMs)
                } else {
                    this@Peripheral.onConnectionStateChange = null
                    invoke.reject(error)
                }
            }
        }
        // Explicitly request the LE transport. Without this, dual-mode
        // peripherals can be connected over BR/EDR which then fails the GATT
        // operations (often surfacing as status 133).
        runOnMain {
            try {
                this.device.connectGatt(activity, false, this.callback, BluetoothDevice.TRANSPORT_LE)
            } catch (e: Exception) {
                Log.e("Peripheral", "Exception during connectGatt: ${e.message}")
                this@Peripheral.onConnectionStateChange = null
                invoke.reject("Exception during connectGatt: ${e.message}")
            }
        }
    }

    @SuppressLint("MissingPermission")
    fun discoverServices(invoke: Invoke) {
        val gatt = this.gatt
        if (gatt == null) {
            invoke.reject("No gatt server connected")
            return
        }
        discoverAttempts = 0
        this.onServicesDiscovered = { success, error ->
            if (success) {
                invoke.resolve()
            } else {
                invoke.reject(error)
            }
            this.onServicesDiscovered = null

        }

        runOnMain {
            if (!gatt.discoverServices()) {
                invoke.reject("failed to start service discovery");
            }
            println("service discovery started")
        }
    }

    fun isConnected(): Boolean {
        return this.connected
    }

    @SuppressLint("MissingPermission")
    fun isBonded(): Boolean {
        return this.device.bondState == BluetoothDevice.BOND_BONDED
    }

    @SuppressLint("MissingPermission")
    fun disconnect(invoke: Invoke) {
        val gatt = this.gatt
        if (gatt == null) {
            this.connected = false
            invoke.resolve()
            return
        }
        // Wait for the disconnect callback before resolving so the caller
        // doesn't immediately try to reconnect while the stack is still
        // cleaning up (which on Android often fails with status 133). The
        // BluetoothGatt is closed inside onConnectionStateChange.
        this.onConnectionStateChange = { _, _ ->
            this@Peripheral.onConnectionStateChange = null
            invoke.resolve()
        }
        runOnMain { gatt.disconnect() }
    }

    class ResCharacteristic(
        private val uuid: String,
        private val properties: Int,
        private val descriptors: List<String>
    ) {
        fun toJson(): JSObject {
            val ret = JSObject()
            ret.put("uuid", uuid)
            ret.put("properties", properties)
            val descriptors = JSONArray()
            for (desc in this.descriptors) {
                descriptors.put(desc)
            }
            ret.put("descriptors", descriptors)
            return ret
        }
    }

    class ResService(
        private val uuid: String,
        private val primary: Boolean,
        private val characs: List<ResCharacteristic>,
    ) {
        fun toJson(): JSObject {
            val ret = JSObject()
            ret.put("uuid", uuid)
            ret.put("primary", primary)
            val characs = JSONArray()
            for (char in this.characs) {
                characs.put(char.toJson())
            }
            ret.put("characs", characs)
            return ret
        }
    }

    fun services(invoke: Invoke) {
        val services = JSONArray()
        for (service in this.services) {
            val characs: MutableList<ResCharacteristic> = mutableListOf()
            for (charac in service.characteristics) {
                characs.add(
                    ResCharacteristic(
                        charac.uuid.toString(),
                        charac.properties,
                        charac.descriptors.map { desc -> desc.uuid.toString() },
                    )
                )
            }
            services.put(
                ResService(
                    service.uuid.toString(),
                    service.type == BluetoothGattService.SERVICE_TYPE_PRIMARY,
                    characs
                ).toJson()
            )
        }
        val res = JSObject()
        res.put("result", services)
        invoke.resolve(res)
    }

    fun setNotifyChannel(channel: Channel) {
        this.notifyChannel = channel;
    }

    @SuppressLint("MissingPermission")
    fun write(invoke: Invoke, args: BleClientPlugin.WriteParams) {
        val key = Pair(args.characteristic!!, args.service!!)
        val charac = this.characteristics[key]
        if (charac == null) {
            invoke.reject("Characterisitc ${args.characteristic} not found")
            return
        }

        val id = this.writeCount.getAndIncrement()
        val timeoutAfter = if (args.timeout > 0) System.currentTimeMillis() + args.timeout else 0L
        val op = PendingWrite(id, key, charac, null, args.data!!, args.withResponse, timeoutAfter = timeoutAfter)
        if (!args.skipWaitingForWriteToComplete) {
            op.invoke = invoke
        }

        var shouldStart = false
        synchronized(this.writeQueueLock) {
            this.writeQueue.addLast(op)
            shouldStart = this.activeWrite == null
        }

        if (args.skipWaitingForWriteToComplete) {
            Log.v(
                "Peripheral",
                "write: skipWaitingForWriteToComplete is true, resolving immediately without waiting for write to complete"
            )
            val start = System.currentTimeMillis()
            invoke.resolve()
            val duration = System.currentTimeMillis() - start
            if (duration > 5) {
                Log.d("Peripheral", "write: skipWaitingForWriteToComplete - invoke.resolve took $duration ms")
            }
        }

        if (shouldStart) {
            runOnMain {
                processWriteQueue()
            }
        }
    }

    @SuppressLint("MissingPermission")
    private fun processWriteQueue() {
        // this function always runs on the main thread; therefore we do not need to synchronize activeWrite
        if (this.activeWrite == null) {
            synchronized(this.writeQueueLock) {
                { // filter timed-out or over-attempt writes out of the queue before starting the next one
                    val to_reject: MutableList<PendingWrite> = mutableListOf()

                    if (this.activeWrite == null) {
                        val queueSize = this.writeQueue.size
                        var iter = 0;
                        while (iter < queueSize) {
                            val pendingWrite = this.writeQueue.removeFirst()
                            if (pendingWrite.attempt > maxAttempts || isWriteTimedOut(pendingWrite)) {
                                to_reject.add(pendingWrite)
                            } else {
                                this.writeQueue.addLast(pendingWrite)
                            }
                            iter++
                        }
                    }

                    for (iter in to_reject) {
                        if (iter.attempt > maxAttempts) {
                            Log.w(
                                "Peripheral",
                                "Discarding queued write to ${iter.key.first} after ${iter.attempt} attempts without success"
                            )
                        } else {
                            Log.w(
                                "Peripheral",
                                "Discarding queued write to ${iter.key.first} that timed out in queue after waiting for ${iter.timeoutAfter - System.currentTimeMillis()} ms"
                            )
                        }
                    }
                }

                if (this.writeQueue.isNotEmpty()) {
                    this.activeWrite = this.writeQueue.removeFirst()
                }
            }
        }

        val current = this.activeWrite ?: return
        // already sent, waiting for onCharacteristicWrite callback.
        if (current.timeSentAt > 0L) {
            val elapsedSinceSent = System.currentTimeMillis() - current.timeSentAt

            val waitUntilRetry =
                if (current.withResponse) {
                    writeCallbackWaitWithResponseMs
                } else {
                    writeCallbackWaitNoResponseMs
                };

            if (elapsedSinceSent < waitUntilRetry) {
                retryHandler.postDelayed(
                    { processWriteQueue() }, waitUntilRetry
                )
                return
            }

            // Callback window elapsed without a response — resend.
            current.timeSentAt = 0L
        }

        val charac = current.characteristic
        val op = current
        // TODO: ensure we correctly clear write queue when disconnecting -- should we trigger a disconnect here if gatt is null??
        val gatt = this.gatt ?: return
        val writeType = if (op.withResponse) {
            BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
        } else {
            BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
        }

        val status: Int = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            gatt.writeCharacteristic(charac, op.data, writeType)
        } else {
            @Suppress("DEPRECATION")
            charac.writeType = writeType
            @Suppress("DEPRECATION")
            charac.value = op.data
            @Suppress("DEPRECATION")
            if (gatt.writeCharacteristic(charac)) {
                BluetoothGatt.GATT_SUCCESS
            } else {
                BluetoothGatt.GATT_FAILURE
            }
        }

        if (status != BluetoothGatt.GATT_SUCCESS) {
            if (status == BluetoothStatusCodes.ERROR_GATT_WRITE_REQUEST_BUSY) {
                discardTimedOutQueuedWrites()
            } else {
                Log.w(
                    "Peripheral",
                    "Failed to start write on ${charac.uuid} (status $status, attempt ${op.attempt}/$maxAttempts)"
                )
                op.attempt += 1
            }

            if (isWriteTimedOut(op)) {
                synchronized(this.writeQueueLock) {
                    if (this.activeWrite == current) {
                        this.activeWrite = null
                    }
                }
                op.invoke?.reject("Write to characteristic ${charac.uuid} timed out")
                return processWriteQueue()
            }

            if (op.attempt < maxAttempts) {
                synchronized(this.writeQueueLock) {
                    if (this.activeWrite == current) {
                        this.activeWrite = op
                    }
                }
                retryHandler.postDelayed({
                    processWriteQueue()
                }, writeRetryDelayMs)
            } else {
                synchronized(this.writeQueueLock) {
                    if (this.activeWrite == current) {
                        this.activeWrite = null
                    }
                }
                op.invoke?.reject(
                    "Failed to start write on characteristic ${charac.uuid} after ${op.attempt} attempts: status $status (${
                        statusCodeName(
                            status
                        )
                    })"
                )
                return processWriteQueue()
            }
        } else {
            op.timeSentAt = System.currentTimeMillis()
            retryHandler.postDelayed({
                processWriteQueue()
            }, 5L)
        }
    }

    private fun callbackWaitMs(op: PendingWrite): Long {
        return if (op.withResponse) {
            writeCallbackWaitWithResponseMs
        } else {
            writeCallbackWaitNoResponseMs
        }
    }

    private fun isWriteTimedOut(op: PendingWrite): Boolean {
        return op.timeoutAfter > 0L && System.currentTimeMillis() >= op.timeoutAfter
    }

    private fun discardTimedOutQueuedWrites() {
        val timedOutWrites: MutableList<PendingWrite> = mutableListOf()
        synchronized(this.writeQueueLock) {
            if (this.writeQueue.isEmpty()) {
                return
            }
            val remaining = ArrayDeque<PendingWrite>()
            while (this.writeQueue.isNotEmpty()) {
                val next = this.writeQueue.removeFirst()
                if (isWriteTimedOut(next)) {
                    timedOutWrites.add(next)
                } else {
                    remaining.addLast(next)
                }
            }
            this.writeQueue.addAll(remaining)
        }

        for (timedOut in timedOutWrites) {
            timedOut.invoke?.reject("Write to characteristic ${timedOut.key.first} timed out in queue while stalled (WRITE_REQUEST_BUSY)")
        }

        if (timedOutWrites.isNotEmpty()) {
            Log.w(
                "Peripheral",
                "Discarded ${timedOutWrites.size} timed-out queued writes while handling WRITE_REQUEST_BUSY"
            )
        }
    }

    @SuppressLint("MissingPermission")
    fun read(invoke: Invoke) {
        val args = invoke.parseArgs(BleClientPlugin.ReadParams::class.java)
        val key = Pair(args.characteristic!!, args.service!!)
        val charac = this.characteristics[key]
        if (charac == null) {
            invoke.reject("Characteristic ${args.characteristic} not found")
            return
        }
        startRead(key, charac, ReadOp(invoke))
    }

    @SuppressLint("MissingPermission")
    private fun startRead(key: Pair<UUID, UUID>, charac: BluetoothGattCharacteristic, op: ReadOp) {
        runOnMain {
            val gatt = this.gatt
            if (gatt == null) {
                op.invoke.reject("No gatt server connected")
                return@runOnMain
            }
            synchronized(this.onReadInvoke) {
                if (this.onReadInvoke[key] != null) {
                    this.onReadInvoke[key]!!.invoke.reject("read was overwritten before finishing")
                }
                this.onReadInvoke[key] = op
            }
            if (!gatt.readCharacteristic(charac)) {
                synchronized(this.onReadInvoke) {
                    this.onReadInvoke.remove(key)
                }
                if (op.attempt < maxAttempts) {
                    val nextAttempt = op.attempt + 1
                    Log.w(
                        "Peripheral",
                        "Failed to start read on ${charac.uuid} (attempt ${op.attempt}/$maxAttempts), retrying"
                    )
                    retryHandler.postDelayed({
                        startRead(key, charac, op.copy(attempt = nextAttempt))
                    }, writeRetryDelayMs)
                } else {
                    op.invoke.reject("Failed to start read on characteristic ${charac.uuid} after ${op.attempt} attempts")
                }
            }
        }
    }

    @SuppressLint("MissingPermission")
    fun subscribe(invoke: Invoke, enabled: Boolean) {
        val args = invoke.parseArgs(BleClientPlugin.ReadParams::class.java)
        val charac = this.characteristics[Pair(args.characteristic!!, args.service!!)]
        if (charac == null) {
            invoke.reject("Characteristic ${args.characteristic} not found")
            return
        }
        val descriptor: BluetoothGattDescriptor? =
            charac.getDescriptor(CLIENT_CHARACTERISTIC_CONFIGURATION_DESCRIPTOR)
        if (descriptor == null) {
            invoke.reject("CCCD descriptor not found on characteristic ${args.characteristic}")
            return
        }
        runOnMain {
            val gatt = this.gatt
            if (gatt == null) {
                invoke.reject("No gatt server connected")
                return@runOnMain
            }
            if (!gatt.setCharacteristicNotification(charac, enabled)) {
                invoke.reject("Failed to set notification status")
                return@runOnMain
            }
            val data =
                if (enabled) BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE else BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE
            startDescriptorWrite(descriptor, DescriptorOp(invoke, data))
        }
    }

    @SuppressLint("MissingPermission")
    private fun startDescriptorWrite(descriptor: BluetoothGattDescriptor, op: DescriptorOp) {
        runOnMain {
            val gatt = this.gatt
            if (gatt == null) {
                op.invoke.reject("No gatt server connected")
                return@runOnMain
            }
            if (this.onDescriptorInvoke != null) {
                this.onDescriptorInvoke!!.invoke.reject("descriptor write was overwritten before finishing")
            }
            this.onDescriptorInvoke = op
            val status: Int = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                gatt.writeDescriptor(descriptor, op.data)
            } else {
                @Suppress("DEPRECATION")
                descriptor.value = op.data
                @Suppress("DEPRECATION")
                if (gatt.writeDescriptor(descriptor)) {
                    BluetoothGatt.GATT_SUCCESS
                } else {
                    BluetoothGatt.GATT_FAILURE
                }
            }
            if (status != BluetoothGatt.GATT_SUCCESS) {
                this.onDescriptorInvoke = null
                if (op.attempt < maxAttempts) {
                    val nextAttempt = op.attempt + 1
                    Log.w(
                        "Peripheral",
                        "Failed to start descriptor write (status $status, attempt ${op.attempt}/$maxAttempts), retrying"
                    )
                    retryHandler.postDelayed({
                        startDescriptorWrite(descriptor, op.copy(attempt = nextAttempt))
                    }, writeRetryDelayMs)
                } else {
                    op.invoke.reject(
                        "Failed to start descriptor write after ${op.attempt} attempts: status $status (${
                            statusCodeName(
                                status
                            )
                        })"
                    )
                }
            }
        }
    }

    @SuppressLint("MissingPermission")
    fun requestMtu(invoke: Invoke, mtu: Int) {
        if (this.onMtuInvoke != null) {
            this.onMtuInvoke!!.reject("mtu request was overwritten before finishing")
        }
        onMtuInvoke = invoke
        runOnMain {
            val gatt = this.gatt
            if (gatt == null) {
                this@Peripheral.onMtuInvoke = null
                invoke.reject("No gatt server connected")
                return@runOnMain
            }
            if (!gatt.requestMtu(mtu)) {
                this@Peripheral.onMtuInvoke = null
                invoke.reject("Failed to request mtu")
            }
        }
    }

    private fun statusCodeName(status: Int): String {
        // Translate the BluetoothStatusCodes / BluetoothGatt error code into a
        // human readable name. Only the GATT-relevant codes are mapped here.
        return when (status) {
            0 -> "SUCCESS"
            1 -> "GATT_INVALID_HANDLE"
            2 -> "GATT_READ_NOT_PERMITTED"
            3 -> "GATT_WRITE_NOT_PERMITTED"
            4 -> "GATT_INVALID_PDU"
            5 -> "GATT_INSUFFICIENT_AUTHENTICATION"
            6 -> "GATT_REQUEST_NOT_SUPPORTED"
            7 -> "GATT_INVALID_OFFSET"
            8 -> "GATT_ERROR"
            13 -> "GATT_CONN_TIMEOUT"
            15 -> "GATT_CONN_TERMINATE_PEER_USER"
            16 -> "GATT_CONN_TERMINATE_LOCAL_HOST"
            19 -> "GATT_CONN_FAIL_ESTABLISH"
            22 -> "GATT_CONN_LMP_TIMEOUT"
            62 -> "GATT_CONN_CANCEL"
            133 -> "GATT_ERROR (133)"
            137 -> "GATT_CONN_TERMINATE_DUE_TO_MIC_FAILURE"
            257 -> "GATT_FAILURE"
            200 -> "ERROR_GATT_WRITE_NOT_ALLOWED"
            201 -> "ERROR_GATT_WRITE_REQUEST_BUSY"
            else -> "UNKNOWN ($status)"
        }
    }
}
