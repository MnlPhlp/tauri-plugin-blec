//! A handle to one connected device.

use std::sync::Arc;

use uuid::Uuid;

use crate::error::Error;
use crate::handler::{Handler, SubscriptionHandler};
use crate::models::{BleDevice, Service, WriteType};

/// A connected device, as returned by [`Handler::connect`] and
/// [`Handler::connection`].
///
/// Only the address and the handler: cloning is cheap, and the methods are
/// the address-taking methods of [`Handler`] for this device. The handle
/// outlives the connection: once the device disconnected the methods return
/// [`Error::DeviceNotConnected`], and they work again after the device was
/// connected again with [`Handler::connect`].
///
/// ```no_run
/// use blec::{models::WriteType, OnDisconnectHandler};
/// use uuid::uuid;
/// # async fn example() -> Result<(), blec::Error> {
/// let handler = blec::get_handler()?;
/// let sensor = handler.connect("00:00:00:00:00:01", OnDisconnectHandler::None, false).await?;
/// let lamp = handler.connect("00:00:00:00:00:02", OnDisconnectHandler::None, false).await?;
/// let value = sensor.recv_data(uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B"), None).await?;
/// lamp.send_data(uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021C"), None, &value, WriteType::WithResponse).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Connection {
    handler: &'static Handler,
    address: Arc<str>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Connection {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.handler, other.handler) && self.address == other.address
    }
}

impl Eq for Connection {}

impl Connection {
    pub(crate) fn new(handler: &'static Handler, address: &str) -> Self {
        Self {
            handler,
            address: address.into(),
        }
    }

    /// The address of the device.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Whether the device is connected right now.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.handler.is_device_connected(&self.address)
    }

    /// The device, with its current properties.
    /// # Errors
    /// Returns an error if the device is not connected
    pub async fn device(&self) -> Result<BleDevice, Error> {
        self.handler.connected_device_at(&self.address).await
    }

    /// See [`Handler::device_mtu`].
    /// # Errors
    /// Returns an error if the device is not connected
    pub async fn mtu(&self) -> Result<u16, Error> {
        self.handler.device_mtu(&self.address).await
    }

    /// See [`Handler::discover_services`].
    /// # Errors
    /// Returns an error if the discovery fails
    pub async fn services(&self) -> Result<Vec<Service>, Error> {
        self.handler.discover_services(&self.address).await
    }

    /// See [`Handler::send_data_to`].
    /// # Errors
    /// Returns an error if the device is not connected, the characteristic is
    /// not available or the write fails
    pub async fn send_data(
        &self,
        c: Uuid,
        service: Option<Uuid>,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<(), Error> {
        self.handler
            .send_data_to(&self.address, c, service, data, write_type)
            .await
    }

    /// See [`Handler::recv_data_from`].
    /// # Errors
    /// Returns an error if the device is not connected, the characteristic is
    /// not available or the read fails
    pub async fn recv_data(&self, c: Uuid, service: Option<Uuid>) -> Result<Vec<u8>, Error> {
        self.handler.recv_data_from(&self.address, c, service).await
    }

    /// See [`Handler::subscribe_to`].
    /// # Errors
    /// Returns an error if the device is not connected, the characteristic is
    /// not available or the subscription fails
    pub async fn subscribe(
        &self,
        c: Uuid,
        service: Option<Uuid>,
        callback: impl Into<SubscriptionHandler>,
    ) -> Result<(), Error> {
        self.handler
            .subscribe_to(&self.address, c, service, callback)
            .await
    }

    /// See [`Handler::unsubscribe_from`].
    /// # Errors
    /// Returns an error if the device is not connected, the characteristic is
    /// not available or the unsubscribe fails
    pub async fn unsubscribe(&self, c: Uuid, service: Option<Uuid>) -> Result<(), Error> {
        self.handler
            .unsubscribe_from(&self.address, c, service)
            .await
    }

    /// See [`Handler::disconnect_device`].
    /// # Errors
    /// Returns an error if the device is not connected
    pub async fn disconnect(&self) -> Result<(), Error> {
        self.handler.disconnect_device(&self.address).await
    }
}
