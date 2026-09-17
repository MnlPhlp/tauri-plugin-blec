@file:JvmName("Bridge")

package com.plugin.blec

import android.app.Activity
import android.app.Application
import android.content.Context
import android.os.Bundle
import android.util.Log
import org.json.JSONObject

/**
 * The entry points the rust side of `blec` calls, and the native callbacks it
 * answers through. See `crates/blec/src/android/bridge.rs`.
 *
 * Everything here is a top-level declaration on purpose: the file compiles to
 * `com.plugin.blec.Bridge` with real `static` (and, for the callbacks, `static
 * native`) methods, which is what `RegisterNatives` and `CallStaticVoidMethod`
 * need. An `object` with `@JvmStatic` would not guarantee that.
 */

internal const val TAG = "blec"

private var plugin: BlecPlugin? = null

/**
 * The activity to request runtime permissions from.
 *
 * Nobody here owns the Activity subclass, so there is no
 * `onRequestPermissionsResult` to hook into: the result is picked up by
 * re-checking the permissions in [onActivityResumed][
 * Application.ActivityLifecycleCallbacks.onActivityResumed], which is also what
 * keeps this up to date. [setActivity] is the fallback for a host that has an
 * activity before any lifecycle callback fired.
 */
internal var currentActivity: Activity? = null
    private set

/** Sets up the plugin. Called from rust right after the dex is loaded. */
fun init(ctx: Context) {
    if (plugin != null) {
        return
    }
    if (ctx is Activity) {
        currentActivity = ctx
    }
    val context = ctx.applicationContext ?: ctx
    plugin = BlecPlugin(context)
    val app = context as? Application
    if (app == null) {
        Log.w(TAG, "no Application context: the current activity can only come from setActivity")
        return
    }
    app.registerActivityLifecycleCallbacks(object : Application.ActivityLifecycleCallbacks {
        override fun onActivityCreated(activity: Activity, state: Bundle?) {}
        override fun onActivityStarted(activity: Activity) {}
        override fun onActivityResumed(activity: Activity) {
            currentActivity = activity
            plugin?.onActivityResumed()
        }
        override fun onActivityPaused(activity: Activity) {}
        override fun onActivityStopped(activity: Activity) {}
        override fun onActivitySaveInstanceState(activity: Activity, state: Bundle) {}
        override fun onActivityDestroyed(activity: Activity) {
            if (currentActivity === activity) {
                currentActivity = null
            }
        }
    })
}

/** Hands over the activity to request permissions from. Called from rust. */
fun setActivity(activity: Activity) {
    currentActivity = activity
}

/**
 * Runs the command `cmd`, answering asynchronously through [Invoke].
 *
 * Returns as soon as the command is started; a command that talks to the radio
 * answers from a gatt callback later. Called from rust on an arbitrary thread.
 */
fun invoke(id: Long, cmd: String, args: String) {
    val invoke = Invoke(id, parseArgs(args))
    val plugin = plugin
    if (plugin == null) {
        invoke.reject("blec: the bridge was never initialized")
        return
    }
    try {
        plugin.handle(cmd, invoke)
    } catch (e: Throwable) {
        Log.e(TAG, "command $cmd failed", e)
        invoke.reject("$cmd: ${e.message ?: e.toString()}")
    }
}

/** Commands without arguments are invoked with a json `null`. */
private fun parseArgs(args: String): JSONObject {
    if (args.isEmpty() || args == "null") {
        return JSONObject()
    }
    return JSONObject(args)
}

/** Completes the rust side's call `id` with `json`. */
external fun nativeResolve(id: Long, json: String)

/** Fails the rust side's call `id`. */
external fun nativeReject(id: Long, message: String)

/** Sends one value to the rust side's channel `id`. */
external fun nativeChannelSend(id: Long, json: String)
