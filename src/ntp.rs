use defmt::*;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};

use crate::wifi::{WifiState, WIFI_CHANNEL};

pub const NTP_SERVER: &str = "pool.ntp.org";
const USEC_IN_SEC: u64 = 1_000_000;

/// Unix timestamp delivered from the NTP task to main.
pub static NTP_TIME_CHANNEL: Channel<CriticalSectionRawMutex, jiff::Timestamp, 1> =
    Channel::new();

#[derive(Clone, Copy, Default)]
struct EmbassyTimestamp {
    micros: u64,
}

impl sntpc::NtpTimestampGenerator for EmbassyTimestamp {
    fn init(&mut self) {
        self.micros = embassy_time::Instant::now().as_micros();
    }

    fn timestamp_sec(&self) -> u64 {
        self.micros / 1_000_000
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        (self.micros % 1_000_000) as u32
    }
}

#[embassy_executor::task]
pub async fn ntp_task(stack: embassy_net::Stack<'static>) -> ! {
    use core::net::{IpAddr, SocketAddr};
    use embassy_net::dns::DnsQueryType;
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    use sntpc::{get_time, NtpContext};
    use sntpc_net_embassy::UdpSocketWrapper;

    let mut wifi_sub = WIFI_CHANNEL.subscriber().unwrap();

    loop {
        loop {
            if wifi_sub.next_message_pure().await == WifiState::NetworkUp {
                break;
            }
        }

        info!("NTP: resolving {}", NTP_SERVER);

        let addrs = match stack.dns_query(NTP_SERVER, DnsQueryType::A).await {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => {
                warn!("NTP: DNS returned no addresses");
                Timer::after(Duration::from_secs(30)).await;
                continue;
            }
            Err(e) => {
                warn!("NTP: DNS failed: {:?}", e);
                Timer::after(Duration::from_secs(30)).await;
                continue;
            }
        };

        let mut rx_meta = [PacketMetadata::EMPTY; 4];
        let mut rx_buffer = [0u8; 512];
        let mut tx_meta = [PacketMetadata::EMPTY; 4];
        let mut tx_buffer = [0u8; 512];
        let mut socket = UdpSocket::new(
            stack,
            &mut rx_meta,
            &mut rx_buffer,
            &mut tx_meta,
            &mut tx_buffer,
        );

        if let Err(e) = socket.bind(1_234) {
            warn!("NTP: bind failed: {:?}", e);
            Timer::after(Duration::from_secs(30)).await;
            continue;
        }

        let socket = UdpSocketWrapper::new(socket);
        let addr: IpAddr = addrs[0].into();
        let dest = SocketAddr::from((addr, 123u16));
        let context = NtpContext::new(EmbassyTimestamp::default());

        match get_time(dest, &socket, context).await {
            Ok(result) => {
                let ntp_usec = (u64::from(result.sec()) * USEC_IN_SEC)
                    + (u64::from(result.sec_fraction()) * USEC_IN_SEC >> 32);
                let ntp_now = jiff::Timestamp::from_microsecond(ntp_usec as i64).unwrap();
                NTP_TIME_CHANNEL.send(ntp_now).await;
            }
            Err(e) => {
                warn!("NTP: get_time failed: {:?}", e);
            }
        }

        loop {
            if wifi_sub.next_message_pure().await == WifiState::Disconnected {
                break;
            }
        }
    }
}
