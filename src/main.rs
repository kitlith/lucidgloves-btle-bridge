use std::collections::BTreeMap;
use std::error::Error;

use anyhow::Context;
use btleplug::api::{Manager as _, Peripheral as _, ScanFilter};
use btleplug::api::{Central, CentralEvent, PeripheralProperties, ValueNotification};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::stream::StreamExt;
use futures::FutureExt;
use tokio::sync::oneshot;
use tokio::net::windows::named_pipe::{self, NamedPipeClient};
use tracing::{error, warn, info};
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

    // TODO: filter to just gloves

    match try_connect(peripheral, properties).await {
        Ok(signal) => *disconnect_signal = signal,
        Err(e) => error!("{:#}", e),
    }
}

async fn try_connect(
    peripheral: Peripheral,
    properties: PeripheralProperties,
) -> Result<Option<oneshot::Sender<()>>, anyhow::Error> {
    peripheral.connect().await.context("Failed to connect")?;
    peripheral.discover_services().await.context("Failed to discover services")?;

    let services = peripheral.services();
    //Characteristic::
    let service = if let Some(service) = services.iter().find(|service| service.uuid == SERIAL_SERVICE) {
        service
    } else {
        error!("Device missing nordic serial service, despite advertising it earlier? Disconnecting.");
        let _ = peripheral.disconnect().await;
        return Ok(None);
    };

    //let rx = service.characteristics.iter().find(|c| c.uuid == RX_CHARACTERISTIC).unwrap();
    let tx = if let Some(c) = service.characteristics.iter().find(|c| c.uuid == TX_CHARACTERISTIC) {
        c
    } else {
        error!("Device missing serial TX characteristic, disconnecting.");
        let _ = peripheral.disconnect().await;
        return Ok(None);
    };

    let tx_stream = peripheral
        .notifications()
        .await
        .context("Failed to create notification stream")?;

    peripheral.subscribe(&tx).await.context("Failed to subscribe to TX characteristic")?;

    let client = named_pipe::ClientOptions::new()
        .pipe_mode(named_pipe::PipeMode::Message)
        .open(OPENGLOVES_PIPE_LEFT) // TODO: differentiate between left and right
        .context("Failed to open named pipe to opengloves")?; 

    let (send, recv) = oneshot::channel();
    tokio::spawn(glove_task(peripheral, properties, tx_stream, client, recv));

    Ok(Some(send))
}

const OPENGLOVES_PIPE_LEFT: &'static str = r"\\.\pipe\vrapplication\input\glove\v2\left";
const OPENGLOVES_PIPE_RIGHT: &'static str = r"\\.\pipe\vrapplication\input\glove\v2\right";

#[repr(C)]
#[derive(Default, Copy, Clone, bytemuck::Zeroable, bytemuck::Pod)]
struct NamedPipeInputData {
    flexion: [[f32; 4]; 5],
    splay: [f32; 5],
    joy_x: f32,
    joy_y: f32,
    // these u8s are intended to be bools
    joy_button: u8,
    trigger_button: u8,
    a_button: u8,
    b_button: u8,
    grab: u8,
    pinch: u8,
    menu: u8,
    calibrate: u8,
    trigger_value: f32,
}

impl NamedPipeInputData {
    // TODO: this function is a bad unwrap just waiting to happen.
    fn parse(mut bytes: &[u8]) -> Self {
        let mut data = Self::default();

        let mut flexion: [[Option<f32>; 4]; 5] = <_>::default();

        while bytes.len() != 0 {
            let key;
            if bytes[0] == b"("[0] {
                let idx = bytes.iter()
                    .cloned()
                    .enumerate()
                    .find(|&(i, b)| b == b")"[0])
                    .unwrap().0;
                key = &bytes[..=idx];
            } else {
                key = &bytes[..=0];
            }
            bytes = &bytes[key.len()..];

            let val_idx = bytes.iter().cloned().enumerate().find(|&(_, b)| !b.is_ascii_digit()).unwrap_or((bytes.len(), 0)).0;
            // SAFETY: consists only of ascii digits, as checked above.
            let val = unsafe { std::str::from_utf8_unchecked(&bytes[..val_idx]) };
            let val = val.parse::<f32>().map(|v| v / 4095.0).ok();
            bytes = &bytes[val_idx..];
    
            match key {
                b"A" => flexion[0].iter_mut().for_each(|e| { e.get_or_insert(val.unwrap()); }),
                b"B" => flexion[1].iter_mut().for_each(|e| { e.get_or_insert(val.unwrap()); }),
                b"C" => flexion[2].iter_mut().for_each(|e| { e.get_or_insert(val.unwrap()); }),
                b"D" => flexion[3].iter_mut().for_each(|e| { e.get_or_insert(val.unwrap()); }),
                b"E" => flexion[4].iter_mut().for_each(|e| { e.get_or_insert(val.unwrap()); }),

                b"F" => data.joy_x = 2.0 * val.unwrap() - 1.0,
                b"G" => data.joy_y = 2.0 * val.unwrap() - 1.0,

                b"H" => data.joy_button = 1,
                b"I" => data.trigger_button = 1,
                b"J" => data.a_button = 1,
                b"K" => data.b_button = 1,
                b"L" => data.grab = 1,
                b"M" => data.pinch = 1,
                b"N" => data.menu = 1,
                b"O" => data.calibrate = 1,
                b"P" => data.trigger_value = val.unwrap(),

                b"(AB)" => data.splay[0] = 2.0 * val.unwrap() - 1.0,
                b"(BB)" => data.splay[1] = 2.0 * val.unwrap() - 1.0,
                b"(CB)" => data.splay[2] = 2.0 * val.unwrap() - 1.0,
                b"(DB)" => data.splay[3] = 2.0 * val.unwrap() - 1.0,
                b"(EB)" => data.splay[4] = 2.0 * val.unwrap() - 1.0,

                b"(AAA)" => flexion[0][0] = Some(val.unwrap()),
                b"(AAB)" => flexion[0][1] = Some(val.unwrap()),
                b"(AAC)" => flexion[0][2] = Some(val.unwrap()),
                b"(AAD)" => flexion[0][3] = Some(val.unwrap()),

                b"(BAA)" => flexion[1][0] = Some(val.unwrap()),
                b"(BAB)" => flexion[1][1] = Some(val.unwrap()),
                b"(BAC)" => flexion[1][2] = Some(val.unwrap()),
                b"(BAD)" => flexion[1][3] = Some(val.unwrap()),

                b"(CAA)" => flexion[2][0] = Some(val.unwrap()),
                b"(CAB)" => flexion[2][1] = Some(val.unwrap()),
                b"(CAC)" => flexion[2][2] = Some(val.unwrap()),
                b"(CAD)" => flexion[2][3] = Some(val.unwrap()),

                b"(DAA)" => flexion[3][0] = Some(val.unwrap()),
                b"(DAB)" => flexion[3][1] = Some(val.unwrap()),
                b"(DAC)" => flexion[3][2] = Some(val.unwrap()),
                b"(DAD)" => flexion[3][3] = Some(val.unwrap()),

                b"(EAA)" => flexion[4][0] = Some(val.unwrap()),
                b"(EAB)" => flexion[4][1] = Some(val.unwrap()),
                b"(EAC)" => flexion[4][2] = Some(val.unwrap()),
                b"(EAD)" => flexion[4][3] = Some(val.unwrap()),

                k => warn!("Unhandled key: {} ({:?})", String::from_utf8_lossy(k), k)
            }
        }

        data.flexion = flexion.map(|i| i.map(|v| v.unwrap_or(0.0)));

        data
    }
}

async fn glove_task(
    peripheral: Peripheral,
    properties: PeripheralProperties,
    tx_stream: futures::stream::BoxStream<'_, ValueNotification>,
    glove_client: NamedPipeClient,
    disconnect: oneshot::Receiver<()>,
) {
    enum Event {
        Notification(ValueNotification),
        //Send(Vec<u8>),
        Disconnect,
    }

    

    let tx_stream = tx_stream.map(|notification| Event::Notification(notification));

    // TODO: named pipe
    let disconnect_stream = futures::stream::once(disconnect.map(|_| Event::Disconnect));

    let mut event_stream = futures::stream::select(tx_stream, disconnect_stream);



    while let Some(event) = event_stream.next().await {
        match event {
            Event::Notification(notification) => {
                let data = NamedPipeInputData::parse(&notification.value);
                glove_client.writable().await.unwrap();
                glove_client.try_write(bytemuck::bytes_of(&data)).unwrap();
            },
            Event::Disconnect => {
                break;
            }
        }
        
    }
}
