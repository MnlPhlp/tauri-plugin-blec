//! An in-process mock of the btleplug backend.
//!
//! Enabled with the `mock` cargo feature (and always for the crate's own tests).
//! The plugin then talks to a simulated radio environment, the [`MockWorld`],
//! instead of the platform Bluetooth stack. Tests and apps script that world:
//! add devices, make them disappear, drop links, delay or fail GATT operations,
//! send notifications, power the adapter off.
//!
//! ```no_run
//! use tauri_plugin_blec::mock::{DeviceSpec, MockWorld, ServiceSpec};
//! use btleplug::api::CharPropFlags;
//! use uuid::uuid;
//!
//! // `Manager::new()` of the mock returns the global world, so an app built
//! // with `--features mock` sees whatever is added here.
//! let device = MockWorld::global().add_device(
//!     DeviceSpec::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01])
//!         .name("mock device")
//!         .service(ServiceSpec::new(uuid!("A07498CA-AD5B-474E-940D-16F1FBE7E8CD")).characteristic(
//!             uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B"),
//!             CharPropFlags::READ | CharPropFlags::WRITE | CharPropFlags::NOTIFY,
//!         )),
//! );
//! // later: simulate a lost link
//! device.drop_link();
//! ```
//!
//! Only desktop targets are supported: on Android the plugin uses its own
//! backend and the `mock` feature is not available.

mod adapter;
mod id;
mod peripheral;
mod world;

pub use adapter::{Adapter, Manager};
pub use id::peripheral_id;
pub use peripheral::Peripheral;
pub use world::{
    ConnectBehaviour, DeviceHandle, DeviceSpec, FailKind, MockWorld, Op, OpBehaviour, ServiceSpec,
};
