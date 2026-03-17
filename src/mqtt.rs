use defmt::*;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Ticker, Timer};
use serde::Deserialize;

use crate::wifi::{WifiState, WIFI_CHANNEL};

pub const MQTT_KEEP_ALIVE_SECONDS: u16 = 60;

/// Latest (north, south) temperatures from MQTT, forwarded to main.
pub static TEMPERATURE_CHANNEL: Channel<CriticalSectionRawMutex, (f32, f32), 1> = Channel::new();

#[derive(Deserialize)]
struct SensorPayload {
    temperature: f32,
}

#[embassy_executor::task]
pub async fn mqtt_task(stack: embassy_net::Stack<'static>) -> ! {
    use embassy_net::tcp::TcpSocket;
    use rust_mqtt::{
        buffer::AllocBuffer,
        client::{
            Client,
            event::Event,
            options::{ConnectOptions, RetainHandling, SubscriptionOptions},
        },
        config::{KeepAlive, SessionExpiryInterval},
        types::{MqttBinary, MqttString, QoS, TopicFilter},
    };

    // Build the client ID once: "rp2040-badger-w-aabbccddeeff"
    let mac = match stack.hardware_address() {
        embassy_net::HardwareAddress::Ethernet(addr) => addr.0,
        _ => [0u8; 6],
    };
    let mut client_id_buf = [0u8; 28]; // 16 prefix + 12 hex digits
    client_id_buf[..16].copy_from_slice(b"rp2040-badger-w-");
    for (i, byte) in mac.iter().enumerate() {
        let hi = byte >> 4;
        let lo = byte & 0xf;
        client_id_buf[16 + i * 2]     = if hi < 10 { b'0' + hi } else { b'a' + hi - 10 };
        client_id_buf[16 + i * 2 + 1] = if lo < 10 { b'0' + lo } else { b'a' + lo - 10 };
    }

    let mut wifi_sub = WIFI_CHANNEL.subscriber().unwrap();
    let mut north_temp: Option<f32> = None;
    let mut south_temp: Option<f32> = None;
    let mut network_up = false;

    'outer: loop {
        // Drain any buffered state updates so we don't block if already up.
        while let Some(state) = wifi_sub.try_next_message_pure() {
            network_up = state == WifiState::NetworkUp;
        }
        if !network_up {
            loop {
                let state = wifi_sub.next_message_pure().await;
                network_up = state == WifiState::NetworkUp;
                if network_up {
                    break;
                }
            }
        }

        let mut tcp_rx = [0u8; 4096];
        let mut tcp_tx = [0u8; 4096];
        let mut socket = TcpSocket::new(stack, &mut tcp_rx, &mut tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(60)));

        if socket
            .connect((embassy_net::Ipv4Address::new(10, 0, 0, 21), 1883u16))
            .await
            .is_err()
        {
            warn!("MQTT: TCP connect failed");
            Timer::after(Duration::from_secs(10)).await;
            continue 'outer;
        }

        let mut buffer = AllocBuffer;
        let mut client = Client::<'_, _, _, 2, 1, 1>::new(&mut buffer);

        let connect_opts = ConnectOptions {
            clean_start: true,
            keep_alive: KeepAlive::Seconds(60),
            session_expiry_interval: SessionExpiryInterval::EndOnDisconnect,
            user_name: Some(MqttString::from_slice("erik").unwrap()),
            password: Some(MqttBinary::from_slice("sensor".as_bytes()).unwrap()),
            will: None,
        };
        if client
            .connect(
                socket,
                &connect_opts,
                Some(MqttString::from_slice(core::str::from_utf8(&client_id_buf).unwrap()).unwrap()),
            )
            .await
            .is_err()
        {
            warn!("MQTT: connect failed");
            Timer::after(Duration::from_secs(10)).await;
            continue 'outer;
        }
        info!("MQTT: connected");

        let sub_opts = SubscriptionOptions {
            qos: QoS::AtMostOnce,
            retain_handling: RetainHandling::AlwaysSend,
            retain_as_published: false,
            no_local: false,
        };

        let _ = client
            .subscribe(
                unsafe {
                    TopicFilter::new_unchecked(
                        MqttString::from_slice("zigbee2mqtt/North").unwrap(),
                    )
                },
                sub_opts,
            )
            .await;
        let _ = client
            .subscribe(
                unsafe {
                    TopicFilter::new_unchecked(
                        MqttString::from_slice("zigbee2mqtt/Balcony South").unwrap(),
                    )
                },
                sub_opts,
            )
            .await;

        let _ = client.ping().await;

        let keep_alive_interval = match client.shared_config().keep_alive {
            KeepAlive::Infinite => Duration::from_secs(MQTT_KEEP_ALIVE_SECONDS as u64),
            KeepAlive::Seconds(s) => Duration::from_secs(s as u64 / 2),
        };
        let mut ticker = Ticker::every(keep_alive_interval);

        loop {
            match select(client.poll(), ticker.next()).await {
                Either::First(Ok(Event::Publish(publish))) => {
                    let topic = publish.topic.as_ref();
                    let payload = publish.message.as_ref();
                    if let Ok((data, _)) =
                        serde_json_core::from_slice::<SensorPayload>(payload)
                    {
                        if topic == "zigbee2mqtt/North" {
                            north_temp = Some(data.temperature);
                        } else if topic == "zigbee2mqtt/Balcony South" {
                            south_temp = Some(data.temperature);
                        }
                        info!("MQTT temp: N={:?} S={:?}", north_temp, south_temp);
                        if let (Some(n), Some(s)) = (north_temp, south_temp) {
                            let _ = TEMPERATURE_CHANNEL.try_send((n, s));
                        }
                    }
                }
                Either::First(Ok(_)) => {}
                Either::First(Err(e)) => {
                    warn!("MQTT: poll error {:?}", e);
                    Timer::after(Duration::from_secs(10)).await;
                    continue 'outer;
                }
                Either::Second(_) => {
                    if let Err(e) = client.ping().await {
                        warn!("MQTT: ping failed {:?}", e);
                        Timer::after(Duration::from_secs(10)).await;
                        continue 'outer;
                    } else {
                        info!("MQTT: ping");
                    }
                }
            }
        }
    }
}
