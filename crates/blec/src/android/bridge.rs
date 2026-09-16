//! The JNI bridge between this crate and its Kotlin side.
//!
//! The Kotlin code lives in `crates/blec/android/` and is compiled to a single
//! `classes.dex` that is committed next to this file and embedded in the
//! binary. At startup [`init`] loads it with an [`InMemoryDexClassLoader`],
//! binds the three native methods the Kotlin side calls back through, and hands
//! it the android `Context`. Consumers therefore need no gradle module, only the
//! manifest permissions.
//!
//! [`InMemoryDexClassLoader`]: https://developer.android.com/reference/dalvik/system/InMemoryDexClassLoader
//!
//! The protocol mirrors the Tauri mobile plugin IPC this replaced:
//!
//! | Direction | Java method |
//! |---|---|
//! | Rust → Kotlin | `static void Bridge.invoke(long id, String cmd, String argsJson)` |
//! | Rust → Kotlin | `static void Bridge.setActivity(Activity a)` |
//! | Kotlin → Rust | `static native void Bridge.nativeResolve(long id, String json)` |
//! | Kotlin → Rust | `static native void Bridge.nativeReject(long id, String message)` |
//! | Kotlin → Rust | `static native void Bridge.nativeChannelSend(long channelId, String json)` |
//!
//! `invoke` returns immediately; the answer arrives on `nativeResolve` or
//! `nativeReject` and completes the oneshot registered under `id`. Streaming
//! results (scan results, adapter events, notifications) go to a [`Channel`],
//! which is just an id the Kotlin side sends to with `nativeChannelSend`.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use jni::objects::{JClass, JClassLoader, JObject, JString};
use jni::refs::{Global, LoaderContext};
use jni::sys::{jlong, jobject};
use jni::{jni_sig, jni_str, native_method, Env, EnvUnowned, JavaVM, NativeMethod, Outcome};
use once_cell::sync::OnceCell;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, trace, warn};

use crate::error::Error;

/// The compiled Kotlin side. Rebuild with `crates/blec/android/build-dex.sh`
/// after changing anything under `crates/blec/android/src`.
const CLASSES_DEX: &[u8] = include_bytes!("classes.dex");

const BRIDGE_CLASS: &str = "com.plugin.blec.Bridge";

/// Added on top of the operation timeout for the IPC call, so the operation
/// timeout in the handler is the one that fires and the IPC timeout stays a
/// safety net for a Kotlin side that never answers at all.
const IPC_TIMEOUT_MARGIN: Duration = Duration::from_secs(5);

struct Bridge {
    vm: JavaVM,
    class: Global<JClass<'static>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    channels: Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>,
    next_id: AtomicU64,
}

static BRIDGE: OnceCell<Bridge> = OnceCell::new();

fn bridge() -> Result<&'static Bridge, Error> {
    BRIDGE
        .get()
        .ok_or_else(|| Error::Android("android bridge not initialized".to_string()))
}

/// Loads the embedded dex, binds the native callbacks and hands the Kotlin side
/// the android `Context`, taking both the `JavaVM` and the `Context` from
/// [`ndk-context`](ndk_context).
///
/// Idempotent: every call after the first one is a no-op. [`crate::init`] calls
/// this, so an app normally does not have to.
///
/// Not every host sets `ndk-context` up: Dioxus does, Tauri (tao 0.35) does not.
/// A host that has a `JNIEnv` and a `Context` of its own, but no `ndk-context`,
/// calls [`init_with`] instead before [`crate::init`].
///
/// # Errors
/// Returns an error if there is no android context (the host has not set up
/// `ndk-context`), or if loading the dex fails. The latter happens on hardened
/// android builds where dynamic code loading is switched off for the app.
pub fn init() -> Result<(), Error> {
    if BRIDGE.get().is_some() {
        return Ok(());
    }
    // ndk-context has no way to ask whether it was initialized: `android_context`
    // panics if not. Turn that into an error that says what to do instead.
    let ctx = std::panic::catch_unwind(ndk_context::android_context).map_err(|_| {
        Error::Android(
            "no android context available: the host did not initialize ndk-context. \
             Call blec::android::init_with with a JNIEnv and a Context before blec::init"
                .to_string(),
        )
    })?;
    if ctx.vm().is_null() || ctx.context().is_null() {
        return Err(Error::Android(
            "no android context available: ndk-context holds null pointers".to_string(),
        ));
    }
    // SAFETY: ndk-context hands out the JavaVM the host stored for this process,
    // which lives as long as the process does.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let raw_context: jobject = ctx.context().cast();

    let class = vm.attach_current_thread(|env| -> Result<Global<JClass<'static>>, Error> {
        // SAFETY: the context is a global reference owned by the host and
        // outlives this local frame.
        let context = unsafe { JObject::from_raw(env, raw_context) };
        load_bridge(env, &context)
    })?;
    store_bridge(vm, class);
    Ok(())
}

/// Like [`init`], but with the `JavaVM` and `Context` coming from the caller
/// instead of `ndk-context`.
///
/// For hosts that never initialize `ndk-context`. Tauri is one: tao runs the
/// app on its own thread and drops the JNI arguments it was started with, so
/// `tauri-plugin-blec` calls this from `wry::prelude::dispatch`, which runs on
/// the main thread with the `Activity` at hand. Any `Context` works; when it is
/// an `Activity`, the Kotlin side also uses it for the runtime permission
/// prompts, so [`set_activity`] is not needed on top.
///
/// Idempotent: every call after the first one is a no-op, including a later
/// [`init`]. Must be called before [`crate::init`], which otherwise falls back
/// to `ndk-context`.
///
/// # Errors
/// Returns an error if loading the dex fails, see [`init`].
///
/// # Safety
/// `env` must be a valid `JNIEnv` pointer for the calling thread and `context`
/// a valid reference to an `android.content.Context` in the current frame.
pub unsafe fn init_with(env: *mut jni::sys::JNIEnv, context: jobject) -> Result<(), Error> {
    if BRIDGE.get().is_some() {
        return Ok(());
    }
    // SAFETY: the caller guarantees `env` belongs to this thread and is at the
    // top of its frame stack.
    let mut unowned = unsafe { EnvUnowned::from_raw(env) };
    let outcome = unowned
        .with_env(|env| -> Result<(JavaVM, Global<JClass<'static>>), Error> {
            let vm = env.get_java_vm()?;
            // SAFETY: the caller guarantees the reference is valid for this frame.
            let context = unsafe { JObject::from_raw(env, context) };
            let class = load_bridge(env, &context)?;
            Ok((vm, class))
        })
        .into_outcome();
    let (vm, class) = match outcome {
        Outcome::Ok(ok) => ok,
        Outcome::Err(e) => return Err(e),
        Outcome::Panic(_) => return Err(Error::Android("init_with panicked".to_string())),
    };
    store_bridge(vm, class);
    Ok(())
}

/// Loads the dex, binds the natives and runs `Bridge.init(context)`. Shared by
/// [`init`] and [`init_with`], which only differ in where the env comes from.
fn load_bridge(env: &mut Env<'_>, context: &JObject<'_>) -> Result<Global<JClass<'static>>, Error> {
    let loader = load_dex(env)?;
    let class = LoaderContext::Loader(&loader)
        .load_class(env, jni_str!("com.plugin.blec.Bridge"), true)
        .map_err(|e| {
            Error::Android(format!("{BRIDGE_CLASS} missing from the embedded dex: {e}"))
        })?;
    // `Java_*` symbols exported by this library are never found for a
    // dex-loaded class: ART resolves them through the class' own loader.
    // Binding them explicitly is the only option.
    //
    // SAFETY: all three are static methods whose rust implementations are
    // generated from the same signatures by `native_method!`.
    unsafe {
        env.register_native_methods(
            &class,
            &[NATIVE_RESOLVE, NATIVE_REJECT, NATIVE_CHANNEL_SEND],
        )?;
    }
    env.call_static_method(
        &class,
        jni_str!("init"),
        jni_sig!((ctx: android.content.Context) -> void),
        &[context.into()],
    )?;
    Ok(env.new_global_ref(&class)?)
}

fn store_bridge(vm: JavaVM, class: Global<JClass<'static>>) {
    let _ = BRIDGE.set(Bridge {
        vm,
        class,
        pending: Mutex::new(HashMap::new()),
        channels: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    });
    debug!("android bridge initialized");
}

/// Loads the embedded dex into a class loader of its own.
///
/// The parent is the *boot* class loader, not the app's. Class loading is
/// parent-first, so with the app's loader as parent every `kotlin.*` class in
/// the dex would be shadowed by the app's own copy of the kotlin stdlib, if it
/// has one (Tauri apps do). That copy is a different version, and it is not
/// the one R8 shrank our code against: R8 inlines stdlib calls and widens
/// package-private stdlib classes (`-allowaccessmodification`) in *our* copy,
/// so resolving them against the app's copy ends in `IllegalAccessError` or
/// `NoSuchMethodError`. The Kotlin side only references the android framework,
/// `java.*`, `org.json` and its own shrunk stdlib, so it needs nothing from the
/// app loader.
fn load_dex<'local>(env: &mut Env<'local>) -> Result<JClassLoader<'local>, Error> {
    let parent = boot_class_loader(env)?;
    // SAFETY: CLASSES_DEX is a `'static` slice in the binary's rodata, so the
    // buffer stays valid for as long as the class loader may read it. The JVM
    // only reads from it.
    let buffer =
        unsafe { env.new_direct_byte_buffer(CLASSES_DEX.as_ptr().cast_mut(), CLASSES_DEX.len()) }?;
    let loader = env
        .new_object(
            jni_str!("dalvik/system/InMemoryDexClassLoader"),
            jni_sig!((buf: java.nio.ByteBuffer, parent: java.lang.ClassLoader) -> void),
            &[(&buffer).into(), (&parent).into()],
        )
        .map_err(|e| {
            Error::Android(format!(
                "could not load the embedded dex: {e}. Loading code at runtime may be blocked \
                 for this app (for example by the GrapheneOS 'dynamic code loading' toggle)"
            ))
        })?;
    Ok(env.cast_local::<JClassLoader>(loader)?)
}

/// The loader the framework classes come from.
///
/// On android `Object.class.getClassLoader()` is the `BootClassLoader`
/// singleton rather than `null` as on the JVM. Should that ever change, the
/// system class loader is the next best thing: it delegates to the boot loader
/// and adds only the (empty) system class path, not the app.
fn boot_class_loader<'local>(env: &mut Env<'local>) -> Result<JClassLoader<'local>, Error> {
    let object = env.find_class(jni_str!("java/lang/Object"))?;
    let mut loader = env
        .call_method(
            &object,
            jni_str!("getClassLoader"),
            jni_sig!(() -> java.lang.ClassLoader),
            &[],
        )?
        .l()?;
    if loader.is_null() {
        warn!("Object.class.getClassLoader() is null, falling back to the system class loader");
        loader = env
            .call_static_method(
                jni_str!("java/lang/ClassLoader"),
                jni_str!("getSystemClassLoader"),
                jni_sig!(() -> java.lang.ClassLoader),
                &[],
            )?
            .l()?;
    }
    Ok(env.cast_local::<JClassLoader>(loader)?)
}

/// Tells the Kotlin side which `Activity` to request runtime permissions from.
///
/// Nobody here owns the `Activity` subclass, so there is no
/// `onRequestPermissionsResult` to hook: the Kotlin side asks this activity and
/// re-checks the permissions when it is resumed again. It also tracks the
/// current activity through `Application.ActivityLifecycleCallbacks`, so this is
/// only needed when that has not seen an activity yet.
///
/// Must be called on the main thread, with an `Activity` reference valid for the
/// current JNI frame. Hosts that use `wry` (Tauri, Dioxus) get both from
/// `wry::prelude::dispatch`.
///
/// # Errors
/// Returns an error if the bridge is not initialized yet.
///
/// # Safety
/// `env` must be a valid `JNIEnv` pointer for the calling thread and `activity`
/// a valid reference to an `android.app.Activity` in the current frame.
pub unsafe fn set_activity(env: *mut jni::sys::JNIEnv, activity: jobject) -> Result<(), Error> {
    let bridge = bridge()?;
    // SAFETY: the caller guarantees `env` belongs to this thread and is at the
    // top of its frame stack.
    let mut unowned = unsafe { EnvUnowned::from_raw(env) };
    let outcome = unowned
        .with_env(|env| -> Result<(), Error> {
            // SAFETY: the caller guarantees the reference is valid for this frame.
            let activity = unsafe { JObject::from_raw(env, activity) };
            env.call_static_method(
                &bridge.class,
                jni_str!("setActivity"),
                jni_sig!((activity: android.app.Activity) -> void),
                &[(&activity).into()],
            )?;
            Ok(())
        })
        .into_outcome();
    match outcome {
        Outcome::Ok(()) => Ok(()),
        Outcome::Err(e) => Err(e),
        Outcome::Panic(_) => Err(Error::Android("set_activity panicked".to_string())),
    }
}

/// Calls a command on the Kotlin side and waits for its answer.
///
/// `timeout` is the operation timeout; the call gives up
/// [`IPC_TIMEOUT_MARGIN`] later so the handler's own timeout is the one that
/// fires and this stays a safety net for a Kotlin side that never answers.
pub(crate) async fn call<P: Serialize, R: DeserializeOwned>(
    cmd: &'static str,
    params: P,
    timeout: Duration,
) -> Result<R, Error> {
    let bridge = bridge()?;
    let args = serde_json::to_string(&params)
        .map_err(|e| Error::Android(format!("could not serialize arguments of {cmd}: {e}")))?;
    let id = bridge.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    bridge
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, tx);

    trace!("invoke {cmd}({args}) as {id}");
    if let Err(e) = invoke(bridge, id, cmd, &args) {
        bridge
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        return Err(e);
    }

    let result = match tokio::time::timeout(timeout + IPC_TIMEOUT_MARGIN, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the android side dropped the call".to_string()),
        Err(_) => {
            bridge
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(Error::Timeout(cmd.to_string()));
        }
    };
    let value = result.map_err(|e| Error::Android(format!("{cmd} failed: {e}")))?;
    serde_json::from_value(value)
        .map_err(|e| Error::Android(format!("could not deserialize the answer of {cmd}: {e}")))
}

fn invoke(bridge: &Bridge, id: u64, cmd: &str, args: &str) -> Result<(), Error> {
    bridge.vm.attach_current_thread(|env| {
        let cmd = JString::from_str(env, cmd)?;
        let args = JString::from_str(env, args)?;
        env.call_static_method(
            &bridge.class,
            jni_str!("invoke"),
            jni_sig!((id: jlong, cmd: java.lang.String, args: java.lang.String) -> void),
            &[
                // ids are allocated by us and never come close to i64::MAX
                #[allow(clippy::cast_possible_wrap)]
                (id as jlong).into(),
                (&cmd).into(),
                (&args).into(),
            ],
        )?;
        Ok(())
    })
}

/// A stream of values the Kotlin side sends to one channel id.
///
/// Registered as long as it is alive; dropping it unregisters the id so a Kotlin
/// side that keeps sending is simply ignored.
pub(crate) struct ChannelReceiver {
    id: u64,
    rx: mpsc::UnboundedReceiver<Value>,
}

impl Drop for ChannelReceiver {
    fn drop(&mut self) {
        if let Some(bridge) = BRIDGE.get() {
            bridge
                .channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.id);
        }
    }
}

impl Stream for ChannelReceiver {
    type Item = Value;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Value>> {
        self.rx.poll_recv(cx)
    }
}

/// The sending end of a [`ChannelReceiver`], as the Kotlin side sees it: a bare
/// id, so `{"onDevice": 7}` is what ends up in the command arguments.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
pub(crate) struct Channel(u64);

/// Allocates a channel the Kotlin side can stream values to.
pub(crate) fn channel() -> Result<(Channel, ChannelReceiver), Error> {
    let bridge = bridge()?;
    let id = bridge.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    bridge
        .channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, tx);
    Ok((Channel(id), ChannelReceiver { id, rx }))
}

const NATIVE_RESOLVE: NativeMethod = native_method! {
    static fn native_resolve(id: jlong, json: java.lang.String) -> void,
};
const NATIVE_REJECT: NativeMethod = native_method! {
    static fn native_reject(id: jlong, message: java.lang.String) -> void,
};
const NATIVE_CHANNEL_SEND: NativeMethod = native_method! {
    static fn native_channel_send(id: jlong, json: java.lang.String) -> void,
};

/// Completes the call `id` with the answer the command produced, `null` for a
/// command that answers nothing.
fn native_resolve(
    env: &mut Env<'_>,
    _class: JClass<'_>,
    id: jlong,
    json: JString<'_>,
) -> Result<(), jni::errors::Error> {
    let json = json.try_to_string(env)?;
    complete(id, parse(&json).map_err(|e| e.to_string()));
    Ok(())
}

/// Fails the call `id`.
fn native_reject(
    env: &mut Env<'_>,
    _class: JClass<'_>,
    id: jlong,
    message: JString<'_>,
) -> Result<(), jni::errors::Error> {
    let message = message.try_to_string(env)?;
    complete(id, Err(message));
    Ok(())
}

/// Delivers one value to the channel `id`.
fn native_channel_send(
    env: &mut Env<'_>,
    _class: JClass<'_>,
    id: jlong,
    json: JString<'_>,
) -> Result<(), jni::errors::Error> {
    let json = json.try_to_string(env)?;
    let Some(bridge) = BRIDGE.get() else {
        return Ok(());
    };
    let value = match parse(&json) {
        Ok(value) => value,
        Err(e) => {
            error!("android sent a malformed channel payload: {e}");
            return Ok(());
        }
    };
    #[allow(clippy::cast_sign_loss)]
    let id = id as u64;
    let channels = bridge
        .channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match channels.get(&id) {
        // The receiver is gone but the entry is still there: the map is only
        // cleaned up when `ChannelReceiver` drops, which cannot race with this.
        Some(tx) => drop(tx.send(value)),
        None => trace!("android sent to the closed channel {id}"),
    }
    Ok(())
}

fn complete(id: jlong, result: Result<Value, String>) {
    let Some(bridge) = BRIDGE.get() else {
        return;
    };
    #[allow(clippy::cast_sign_loss)]
    let id = id as u64;
    let pending = bridge
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    match pending {
        Some(tx) => drop(tx.send(result)),
        // The call timed out before the answer arrived, or kotlin answered twice.
        None => warn!("android answered the unknown call {id}"),
    }
}

/// An empty payload is a command that answers nothing, which serde reads as
/// `null` and deserializes into `()`.
fn parse(json: &str) -> Result<Value, serde_json::Error> {
    if json.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(json)
}
