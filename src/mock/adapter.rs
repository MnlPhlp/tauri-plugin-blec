//! The mock `Central` and `Manager`.

use std::pin::Pin;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use btleplug::api::{Central, CentralEvent, CentralState, RetrievePeripheralsOptions, ScanFilter};
use btleplug::platform::PeripheralId;
use futures::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use super::peripheral::Peripheral;
use super::world::MockWorld;

/// The adapter of a [`MockWorld`].
#[derive(Clone, Debug)]
pub struct Adapter {
    world: MockWorld,
}

impl Adapter {
    pub(super) fn new(world: MockWorld) -> Self {
        Self { world }
    }

    /// The world this adapter belongs to.
    #[must_use]
    pub fn world(&self) -> &MockWorld {
        &self.world
    }
}

#[async_trait]
impl Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> btleplug::Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        let stream = BroadcastStream::new(self.world.inner.events.subscribe())
            // a lagging receiver simply misses events, like a real adapter
            .filter_map(|event| async move { event.ok() });
        Ok(Box::pin(stream))
    }

    async fn start_scan(&self, _filter: ScanFilter) -> btleplug::Result<()> {
        if self.world.adapter_state() == CentralState::PoweredOff {
            return Err(btleplug::Error::NotConnected);
        }
        self.world.inner.scanning.store(true, Ordering::Release);
        for peripheral in self.world.in_range_devices() {
            // an advertisement re-registers a peripheral the backend forgot
            peripheral.inner().set_known(true);
            self.world.emit(CentralEvent::DeviceDiscovered(
                peripheral.inner().id.clone(),
            ));
        }
        Ok(())
    }

    async fn stop_scan(&self) -> btleplug::Result<()> {
        self.world.inner.scanning.store(false, Ordering::Release);
        Ok(())
    }

    async fn peripherals(&self) -> btleplug::Result<Vec<Peripheral>> {
        Ok(self.world.in_range_devices())
    }

    /// Identifiers are looked up in the whole world, like
    /// `retrievePeripherals(withIdentifiers:)` reads the system's record of
    /// every peripheral it ever saw; services only match connected devices,
    /// like `retrieveConnectedPeripherals(withServices:)`. Everything returned
    /// is re-registered, so a peripheral the backend forgot becomes connectable
    /// again.
    async fn retrieve_peripherals(
        &self,
        options: RetrievePeripheralsOptions,
    ) -> btleplug::Result<Vec<Peripheral>> {
        if options.identifiers.is_none() && options.services.is_none() {
            return Err(btleplug::Error::NotSupported(
                "retrieve_peripherals".to_string(),
            ));
        }
        let identifiers = options.identifiers.unwrap_or_default();
        let services = options.services.unwrap_or_default();
        let mut retrieved = vec![];
        for peripheral in self.world.all_devices() {
            let device = peripheral.inner();
            let by_service = device.is_connected()
                && services
                    .iter()
                    .any(|service| device.advertised_services().contains(service));
            if !identifiers.contains(&device.id) && !by_service {
                continue;
            }
            device.set_known(true);
            retrieved.push(peripheral);
        }
        Ok(retrieved)
    }

    async fn peripheral(&self, id: &PeripheralId) -> btleplug::Result<Peripheral> {
        self.world
            .device_by_id(id)
            .filter(|p| p.inner().is_in_range())
            .ok_or(btleplug::Error::DeviceNotFound)
    }

    async fn add_peripheral(&self, _address: &PeripheralId) -> btleplug::Result<Peripheral> {
        Err(btleplug::Error::NotSupported(
            "mock: add_peripheral".to_string(),
        ))
    }

    async fn clear_peripherals(&self) -> btleplug::Result<()> {
        Ok(())
    }

    async fn adapter_info(&self) -> btleplug::Result<String> {
        Ok("mock adapter".to_string())
    }

    async fn adapter_state(&self) -> btleplug::Result<CentralState> {
        Ok(self.world.adapter_state())
    }
}

/// The mock `Manager`: hands out the adapter of [`MockWorld::global`].
#[derive(Debug, Default)]
pub struct Manager;

impl Manager {
    /// Never fails; the signature mirrors `btleplug::platform::Manager::new`.
    pub async fn new() -> btleplug::Result<Self> {
        Ok(Self)
    }
}

#[async_trait]
impl btleplug::api::Manager for Manager {
    type Adapter = Adapter;

    async fn adapters(&self) -> btleplug::Result<Vec<Adapter>> {
        Ok(vec![MockWorld::global().adapter()])
    }
}
