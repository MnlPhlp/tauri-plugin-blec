package com.plugin.blec

import org.json.JSONObject
import java.util.UUID

/**
 * One command call from the rust side, answered exactly once.
 *
 * Replaces `app.tauri.plugin.Invoke`. Arguments are plain `org.json` instead of
 * jackson-mapped classes, so the argument classes below parse themselves.
 */
class Invoke(private val id: Long, val args: JSONObject) {
    private var answered = false

    fun resolve() {
        resolve(null)
    }

    fun resolve(data: JSONObject?) {
        if (answered) {
            return
        }
        answered = true
        nativeResolve(id, data?.toString() ?: "null")
    }

    fun reject(message: String) {
        if (answered) {
            return
        }
        answered = true
        nativeReject(id, message)
    }
}

/**
 * The sending end of a stream the rust side listens on, identified by the id it
 * put into the command arguments.
 */
class Channel(private val id: Long) {
    fun send(data: JSONObject) {
        nativeChannelSend(id, data.toString())
    }

    companion object {
        /** Reads a channel id the rust side serialized as a bare number. */
        fun from(args: JSONObject, key: String): Channel = Channel(args.getLong(key))
    }
}

internal fun JSONObject.uuidOrNull(key: String): UUID? =
    if (isNull(key)) null else UUID.fromString(getString(key))

/** `&[u8]` serializes as a json array of numbers. */
internal fun JSONObject.byteArrayOrNull(key: String): ByteArray? {
    if (isNull(key)) {
        return null
    }
    val array = getJSONArray(key)
    return ByteArray(array.length()) { array.getInt(it).toByte() }
}

class ConnectParams(val address: String) {
    companion object {
        fun from(args: JSONObject) = ConnectParams(args.getString("address"))
    }
}

class NotifyParams(val address: String, val channel: Channel) {
    companion object {
        fun from(args: JSONObject) =
            NotifyParams(args.getString("address"), Channel.from(args, "channel"))
    }
}

class WriteParams(
    val address: String,
    val characteristic: UUID?,
    val service: UUID?,
    val data: ByteArray?,
    val withResponse: Boolean,
    /** A write that is not sent within this many ms is dropped; 0 means never. */
    val timeout: Int,
    /**
     * Enqueue the write and answer immediately. The write still happens as long
     * as the connection holds, but nobody learns when.
     */
    val skipWaitingForWriteToComplete: Boolean,
) {
    companion object {
        fun from(args: JSONObject) = WriteParams(
            args.getString("address"),
            args.uuidOrNull("characteristic"),
            args.uuidOrNull("service"),
            args.byteArrayOrNull("data"),
            args.optBoolean("withResponse", true),
            args.optInt("timeout", 0),
            args.optBoolean("skipWaitingForWriteToComplete", false),
        )
    }
}

class ReadParams(val address: String, val characteristic: UUID?, val service: UUID?) {
    companion object {
        fun from(args: JSONObject) = ReadParams(
            args.getString("address"),
            args.uuidOrNull("characteristic"),
            args.uuidOrNull("service"),
        )
    }
}

class CheckPermissionsParams(val allowIbeacons: Boolean, val askIfDenied: Boolean) {
    companion object {
        fun from(args: JSONObject) = CheckPermissionsParams(
            args.optBoolean("allowIbeacons", false),
            args.optBoolean("askIfDenied", false),
        )
    }
}

class MtuParams(val address: String, val mtu: Int) {
    companion object {
        fun from(args: JSONObject) =
            MtuParams(args.getString("address"), args.optInt("mtu", 517))
    }
}
