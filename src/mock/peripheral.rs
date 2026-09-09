//! The mock `Peripheral`.

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use btleplug::api::{
    BDAddr, Characteristic, Descriptor, PeripheralProperties, Service, ValueNotification, WriteType,
};
use btleplug::platform::PeripheralId;
use futures::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;
use tracing::debug;

use super::world::{ConnectBehaviour, DeviceInner, NotifEvent, Op, OpBehaviour};

/// A device as seen by the central. Cloning shares the device.
#[derive(Clone, Debug)]
pub struct Peripheral {
    inner: Arc<DeviceInner>,
}

impl Peripheral {
    pub(super) fn new(inner: Arc<DeviceInner>) -> Self {
        Self { inner }
    }

    pub(super) fn inner(&self) -> &DeviceInner {
        &self.inner
    }

    fn require_connected(&self) -> btleplug::Result<()> {
        if self.inner.is_connected() {
            Ok(())
        } else {
            Err(btleplug::Error::NotConnected)
        }
    }

    fn require_characteristic(&self, characteristic: &Characteristic) -> btleplug::Result<()> {
        self.inner
            .find_characteristic(characteristic.service_uuid, characteristic.uuid)
            .map(|_| ())
            .ok_or(btleplug::Error::NoSuchCharacteristic)
    }

    fn next_connect_behaviour(&self) -> ConnectBehaviour {
        let mut behaviour = self.inner.behaviour.lock().unwrap();
        behaviour
            .connect_queue
            .pop_front()
            .unwrap_or_else(|| behaviour.connect.clone())
    }

    fn behaviour<F: FnOnce(&super::world::Behaviour) -> OpBehaviour>(
        &self,
        pick: F,
    ) -> OpBehaviour {
        pick(&self.inner.behaviour.lock().unwrap())
    }
}

/// Runs an operation's behaviour: waits, fails or hangs as configured.
async fn apply(behaviour: OpBehaviour) -> btleplug::Result<()> {
    match behaviour {
        OpBehaviour::Ok => Ok(()),
        OpBehaviour::Delay(delay) => {
            tokio::time::sleep(delay).await;
            Ok(())
        }
        OpBehaviour::Fail(kind) => Err(kind.to_error()),
        OpBehaviour::Hang => std::future::pending().await,
    }
}

#[async_trait]
impl btleplug::api::Peripheral for Peripheral {
    fn id(&self) -> PeripheralId {
        self.inner.id.clone()
    }

    fn address(&self) -> BDAddr {
        self.inner.spec.address
    }

    fn mtu(&self) -> u16 {
        self.inner.spec.mtu
    }

    async fn properties(&self) -> btleplug::Result<Option<PeripheralProperties>> {
        let spec = &self.inner.spec;
        Ok(Some(PeripheralProperties {
            address: spec.address,
            address_type: None,
            local_name: spec.name.clone(),
            advertisement_name: spec.name.clone(),
            tx_power_level: spec.tx_power_level,
            // no advertisements are received from a device that is out of range
            rssi: if self.inner.is_in_range() {
                spec.rssi
            } else {
                None
            },
            manufacturer_data: spec.manufacturer_data.clone(),
            service_data: spec.service_data.clone(),
            services: self.inner.advertised_services(),
            class: None,
        }))
    }

    fn services(&self) -> BTreeSet<Service> {
        if self.inner.discovered.load(Ordering::Acquire) {
            self.inner.gatt.clone()
        } else {
            BTreeSet::new()
        }
    }

    async fn is_connected(&self) -> btleplug::Result<bool> {
        Ok(self.inner.is_connected())
    }

    async fn connect(&self) -> btleplug::Result<()> {
        self.inner.record(Op::Connect);
        if !self.inner.is_in_range() {
            debug!("mock: connect to out of range device {}", self.inner.id);
            return Err(btleplug::Error::DeviceNotFound);
        }
        if self.inner.is_connected() {
            return Ok(());
        }
        let behaviour = self.next_connect_behaviour();
        debug!("mock: connect {} with {behaviour:?}", self.inner.id);
        match behaviour {
            ConnectBehaviour::Ok => self.inner.establish_link(),
            ConnectBehaviour::OkAfter(delay) => {
                tokio::time::sleep(delay).await;
                self.inner.establish_link();
            }
            ConnectBehaviour::Fail(kind) => return Err(kind.to_error()),
            ConnectBehaviour::Hang => {}
            ConnectBehaviour::OkThenDrop(delay) => {
                self.inner.establish_link();
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    inner.drop_link(true);
                });
            }
        }
        Ok(())
    }

    async fn disconnect(&self) -> btleplug::Result<()> {
        self.inner.record(Op::Disconnect);
        let (behaviour, emits_event) = {
            let b = self.inner.behaviour.lock().unwrap();
            (b.disconnect.clone(), b.disconnect_emits_event)
        };
        apply(behaviour).await?;
        self.inner.drop_link(emits_event);
        Ok(())
    }

    async fn discover_services(&self) -> btleplug::Result<()> {
        self.inner.record(Op::DiscoverServices);
        self.require_connected()?;
        apply(self.behaviour(|b| b.discover.clone())).await?;
        self.require_connected()?;
        self.inner.discovered.store(true, Ordering::Release);
        Ok(())
    }

    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> btleplug::Result<()> {
        self.inner
            .record(Op::Write(characteristic.uuid, data.to_vec(), write_type));
        self.require_connected()?;
        self.require_characteristic(characteristic)?;
        apply(self.behaviour(|b| b.write.clone())).await?;
        self.require_connected()?;
        self.inner.values.lock().unwrap().insert(
            (characteristic.service_uuid, characteristic.uuid),
            data.to_vec(),
        );
        Ok(())
    }

    async fn read(&self, characteristic: &Characteristic) -> btleplug::Result<Vec<u8>> {
        self.inner.record(Op::Read(characteristic.uuid));
        self.require_connected()?;
        self.require_characteristic(characteristic)?;
        apply(self.behaviour(|b| b.read.clone())).await?;
        self.require_connected()?;
        Ok(self
            .inner
            .values
            .lock()
            .unwrap()
            .get(&(characteristic.service_uuid, characteristic.uuid))
            .cloned()
            .unwrap_or_default())
    }

    async fn subscribe(&self, characteristic: &Characteristic) -> btleplug::Result<()> {
        self.inner.record(Op::Subscribe(characteristic.uuid));
        self.require_connected()?;
        self.require_characteristic(characteristic)?;
        apply(self.behaviour(|b| b.subscribe.clone())).await?;
        self.require_connected()?;
        self.inner
            .subscriptions
            .lock()
            .unwrap()
            .insert((characteristic.service_uuid, characteristic.uuid));
        Ok(())
    }

    async fn unsubscribe(&self, characteristic: &Characteristic) -> btleplug::Result<()> {
        self.inner.record(Op::Unsubscribe(characteristic.uuid));
        self.require_connected()?;
        self.require_characteristic(characteristic)?;
        apply(self.behaviour(|b| b.subscribe.clone())).await?;
        self.inner
            .subscriptions
            .lock()
            .unwrap()
            .remove(&(characteristic.service_uuid, characteristic.uuid));
        Ok(())
    }

    async fn notifications(
        &self,
    ) -> btleplug::Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        let stream = BroadcastStream::new(self.inner.notifications.subscribe())
            // lagging = lost notifications, keep going
            .filter_map(|event| async move { event.ok() })
            .take_while(|event| {
                let keep = !matches!(event, NotifEvent::End);
                async move { keep }
            })
            .filter_map(|event| async move {
                match event {
                    NotifEvent::Value(value) => Some(value),
                    NotifEvent::End => None,
                }
            });
        Ok(Box::pin(stream))
    }

    async fn write_descriptor(
        &self,
        _descriptor: &Descriptor,
        _data: &[u8],
    ) -> btleplug::Result<()> {
        Err(btleplug::Error::NotSupported(
            "mock: write_descriptor".to_string(),
        ))
    }

    async fn read_descriptor(&self, _descriptor: &Descriptor) -> btleplug::Result<Vec<u8>> {
        Err(btleplug::Error::NotSupported(
            "mock: read_descriptor".to_string(),
        ))
    }
}
