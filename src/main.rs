use std::collections::BTreeMap;
use std::error::Error;

use btleplug::api::{Manager as _, Peripheral as _, ScanFilter};
use btleplug::api::{Central, CentralEvent, PeripheralProperties, ValueNotification};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::stream::StreamExt;
use futures::FutureExt;
use tokio::sync::oneshot;
use tracing::{error, info};
use uuid::{uuid, Uuid};

const SERIAL_SERVICE: Uuid = uuid!("6E400001-B5A3-F393-E0A9-E50E24DCCA9E");
//const RX_CHARACTERISTIC: Uuid = uuid!("6E400002-B5A3-F393-E0A9-E50E24DCCA9E");
const TX_CHARACTERISTIC: Uuid = uuid!("6E400003-B5A3-F393-E0A9-E50E24DCCA9E");

// const SERIAL_FILTER: ScanFilter = ScanFilter { services: vec![ SERIAL_SERVICE ] };

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(tracing::Level::INFO)
        // completes the builder.
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("setting default subscriber failed");

    let manager = Manager::new().await?;

    let adapters = manager.adapters().await?;
    let adapter = adapters.get(0).ok_or("No BTLE adapter found.")?;

    let mut events = adapter.events().await?;

    adapter.start_scan(ScanFilter { services: vec![ SERIAL_SERVICE ] }).await?;
    //adapter.start_scan(ScanFilter::default()).await?;

    //central.peripheral(id)

    let mut tried_addresses = BTreeMap::new();

    // TODO: handle quit

    while let Some(event) = events.next().await {
        match event {
            CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => {
                maybe_add_peripheral(&id, &adapter, &mut tried_addresses).await;
            }
            CentralEvent::DeviceDisconnected(id) => {
                if let Some(Some(disconnect_signal)) = tried_addresses.remove(&id) {
                    let _ = disconnect_signal.send(()); // notify task of disconnect
                }
            }
            // CentralEvent::ManufacturerDataAdvertisement { id, manufacturer_data } => {}
            // CentralEvent::ServiceDataAdvertisement { id, service_data } => {}
            // CentralEvent::ServicesAdvertisement { id, services } => {}
            _ => {}
        }
    }

    Ok(())
}

async fn maybe_add_peripheral(
    id: &PeripheralId,
    adapter: &Adapter,
    tried_addresses: &mut BTreeMap<PeripheralId, Option<oneshot::Sender<()>>>,
) {
    if tried_addresses.contains_key(id) {
        return;
    }

    let disconnect_signal = tried_addresses.entry(id.clone()).or_insert(None);

    let peripheral = if let Ok(peripheral) = adapter.peripheral(id).await {
        peripheral
    } else {
        error!("Peripheral with address {:?} not found.", id);
        return;
    };

    let properties = if let Ok(Some(properties)) = peripheral.properties().await {
        properties
    } else {
        error!("Cannot retreive peripheral properties for {:?}.", id);
        return;
    };

    if !properties.services.contains(&SERIAL_SERVICE) {
        return;
    }

    info!("Found nordic serial device with name: {:?}", properties.local_name);

    // TODO: other filters

    let (send, recv) = oneshot::channel();
    *disconnect_signal = Some(send);
    tokio::spawn(glove_task(peripheral, properties, recv));
}

async fn glove_task(
    peripheral: Peripheral,
    properties: PeripheralProperties,
    disconnect: oneshot::Receiver<()>,
) {
    peripheral.connect().await.unwrap();
    peripheral.discover_services().await.unwrap();

    let services = peripheral.services();
    //Characteristic::
    let service = if let Some(service) = services.iter().find(|service| service.uuid == SERIAL_SERVICE) {
        service
    } else {
        return;
    };

    //let rx = service.characteristics.iter().find(|c| c.uuid == RX_CHARACTERISTIC).unwrap();
    let tx = service.characteristics.iter().find(|c| c.uuid == TX_CHARACTERISTIC).unwrap();

    enum Event {
        Notification(ValueNotification),
        Disconnect,
    }

    peripheral.subscribe(&tx).await.unwrap();
    let tx_stream = peripheral
        .notifications()
        .await
        .unwrap()
        .map(|notification| Event::Notification(notification));

    // TODO: named pipe
    let disconnect_stream = futures::stream::once(disconnect.map(|_| Event::Disconnect));

    let mut event_stream = futures::stream::select(tx_stream, disconnect_stream);

    while let Some(event) = event_stream.next().await {
        match event {
            Event::Notification(notification) => {
                info!(name = properties.local_name, "Received serial bytes: {}", String::from_utf8_lossy(&notification.value));
            },
            Event::Disconnect => {
                break;
            }
        }
        
    }
}
