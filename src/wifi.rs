use cyw43::JoinOptions;
use cyw43_pio::PioSpi;
use defmt::*;
use embassy_futures::select::{select, Either};
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pubsub::PubSubChannel;
use embassy_time::{Duration, Timer};

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

#[derive(Clone, Copy, PartialEq)]
pub enum WifiState {
    Disconnected,
    Joining,
    Connected,
    LinkUp,
    NetworkUp,
}

impl WifiState {
    pub fn to_str(&self) -> &'static str {
        match self {
            WifiState::Disconnected => "Disconnected",
            WifiState::Joining      => "Joining",
            WifiState::Connected    => "Connected",
            WifiState::LinkUp       => "LinkUp",
            WifiState::NetworkUp    => "NetworkUp",
        }
    }
}

impl defmt::Format for WifiState {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "{}", self.to_str());
    }
}

/// Capacity: 4 queued messages, 4 subscribers, 1 publisher.
pub static WIFI_CHANNEL: PubSubChannel<CriticalSectionRawMutex, WifiState, 4, 4, 1> =
    PubSubChannel::new();

#[embassy_executor::task]
pub async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub async fn wifi_task(
    mut control: cyw43::Control<'static>,
    stack: embassy_net::Stack<'static>,
) -> ! {
    let publisher = WIFI_CHANNEL.publisher().unwrap();
    info!("Starting Wi-Fi task");
    loop {
        publisher.publish(WifiState::Joining).await;
        loop {
            info!("Joining Wi-Fi network...");
            match select(
                control.join(WIFI_SSID, JoinOptions::new(WIFI_PASSWORD.as_bytes())),
                Timer::after(Duration::from_secs(30)),
            )
            .await
            {
                Either::First(Ok(_)) => break,
                Either::First(Err(err)) => {
                    info!("join failed with status={}", err.status);
                    publisher.publish(WifiState::Disconnected).await;
                    Timer::after(Duration::from_secs(2)).await;
                    publisher.publish(WifiState::Joining).await;
                }
                Either::Second(_) => {
                    warn!("Wi-Fi join timed out, retrying");
                    publisher.publish(WifiState::Disconnected).await;
                    Timer::after(Duration::from_secs(2)).await;
                    publisher.publish(WifiState::Joining).await;
                }
            }
        }
        publisher.publish(WifiState::Connected).await;

        info!("Waiting for link...");
        stack.wait_link_up().await;
        publisher.publish(WifiState::LinkUp).await;

        info!("Waiting for DHCP...");
        stack.wait_config_up().await;
        publisher.publish(WifiState::NetworkUp).await;

        if let Some(cfg) = stack.config_v4() {
            info!("IP address: {}", cfg.address);
        }

        stack.wait_link_down().await;
        info!("Link lost, reconnecting...");
        publisher.publish(WifiState::Disconnected).await;
    }
}
