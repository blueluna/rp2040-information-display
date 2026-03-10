#![no_std]
#![no_main]

use cyw43::JoinOptions;
use cyw43_pio::{DEFAULT_CLOCK_DIVIDER, PioSpi};
use defmt::*;
use efmt::uformat;
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::spi::{self, Spi};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::pubsub::PubSubChannel;
use embassy_time::{Delay, Duration, Timer};
use embedded_graphics::{
    mono_font::{ascii::*, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder},
    text::Text,
};
use static_cell::StaticCell;
use u8g2_fonts::fonts::u8g2_font_logisoso38_tn;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use uc8151::asynch::Uc8151;
use uc8151::LUT;
use uc8151::WIDTH;
use uc8151::HEIGHT;

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    I2C0_IRQ => embassy_rp::i2c::InterruptHandler<embassy_rp::peripherals::I2C0>;
});

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const TIMEZONE: jiff::tz::TimeZone = jiff::tz::get!("Europe/Stockholm");

#[derive(Clone, Copy, PartialEq)]
pub enum WifiState {
    Disconnected,
    Joining,
    Connected,
    LinkUp,
    NetworkUp,
}

impl WifiState {
    /// Return a human‑readable string for the current state.
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
static WIFI_CHANNEL: PubSubChannel<CriticalSectionRawMutex, WifiState, 4, 4, 1> =
    PubSubChannel::new();

/// Unix timestamp delivered from the NTP task to main.
static NTP_TIME_CHANNEL: Channel<CriticalSectionRawMutex, jiff::Timestamp, 1> = Channel::new();

const NTP_SERVER: &str = "pool.ntp.org";

/// Microseconds in a second
const USEC_IN_SEC: u64 = 1_000_000;

/// Timestamp generator for sntpc using embassy_time as the reference clock.
/// The values are relative (uptime), which is fine for the originate-timestamp field.
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

fn unix_to_primitive_datetime(timestamp: jiff::Timestamp) -> Option<time::PrimitiveDateTime> {
    let dt = jiff::tz::Offset::UTC.to_datetime(timestamp);
    let date = time::Date::from_calendar_date(
        dt.year() as i32,
        time::Month::try_from(dt.month() as u8).ok()?,
        dt.day() as u8,
    )
    .ok()?;
    let t = time::Time::from_hms(dt.hour() as u8, dt.minute() as u8, dt.second() as u8).ok()?;
    Some(time::PrimitiveDateTime::new(date, t))
}

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn wifi_task(
    mut control: cyw43::Control<'static>,
    stack: embassy_net::Stack<'static>,
) -> ! {
    let publisher = WIFI_CHANNEL.publisher().unwrap();
    info!("Starting Wi-Fi task");
    loop {
        // --- Join ---
        publisher.publish(WifiState::Joining).await;
        loop {
            info!("Joining Wi-Fi network...");
            match control
                .join(WIFI_SSID, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
                .await
            {
                Ok(_) => break,
                Err(err) => {
                    info!("join failed with status={}", err.status);
                    publisher.publish(WifiState::Disconnected).await;
                    Timer::after(Duration::from_secs(2)).await;
                    publisher.publish(WifiState::Joining).await;
                }
            }
        }
        publisher.publish(WifiState::Connected).await;

        // --- Link up ---
        info!("Waiting for link...");
        stack.wait_link_up().await;
        publisher.publish(WifiState::LinkUp).await;

        // --- DHCP ---
        info!("Waiting for DHCP...");
        stack.wait_config_up().await;
        publisher.publish(WifiState::NetworkUp).await;

        if let Some(cfg) = stack.config_v4() {
            info!("IP address: {}", cfg.address);
        }

        // --- Monitor until connection is lost ---
        stack.wait_link_down().await;
        info!("Link lost, reconnecting...");
        publisher.publish(WifiState::Disconnected).await;
    }
}

#[embassy_executor::task]
async fn ntp_task(stack: embassy_net::Stack<'static>) -> ! {
    use core::net::{IpAddr, SocketAddr};
    use embassy_net::dns::DnsQueryType;
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    use sntpc::{get_time, NtpContext};
    use sntpc_net_embassy::UdpSocketWrapper;

    let mut wifi_sub = WIFI_CHANNEL.subscriber().unwrap();

    loop {
        // Wait until the network is up.
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
                let ntp_usec = (u64::from(result.sec()) * USEC_IN_SEC) + (u64::from(result.sec_fraction()) * USEC_IN_SEC >> 32);
                let ntp_now = jiff::Timestamp::from_microsecond(ntp_usec as i64).unwrap();
                NTP_TIME_CHANNEL.send(ntp_now).await;
            }
            Err(e) => {
                warn!("NTP: get_time failed: {:?}", e);
            }
        }

        // Wait for link to drop before attempting again.
        loop {
            if wifi_sub.next_message_pure().await == WifiState::Disconnected {
                break;
            }
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    info!("Starting Wi-Fi + DHCP example");

    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;

    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    match spawner.spawn(cyw43_task(runner)) {
        Ok(_) => {}
        Err(err) => {
            error!("Failed to spawn cyw43 task: {}", err);
        }
    }

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());
    let seed = rng.next_u64();

    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) =
        embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);
    match spawner.spawn(net_task(runner)) {
        Ok(_) => {}
        Err(err) => {
            error!("Failed to spawn net task: {}", err);
        }
    }

    let miso = p.PIN_16;
    let mosi = p.PIN_19;
    let clk = p.PIN_18;
    let dc = p.PIN_20;
    let cs = p.PIN_17;
    let busy = p.PIN_26;
    let reset = p.PIN_21;
    let power = p.PIN_10;

    let btn_up = p.PIN_15;
    let btn_down = p.PIN_11;
    let btn_a = p.PIN_12;
    let btn_b = p.PIN_13;
    let btn_c = p.PIN_14;

    let led = p.PIN_22;

    let reset = Output::new(reset, Level::Low);
    let _power = Output::new(power, Level::Low);

    let dc = Output::new(dc, Level::Low);
    let cs = Output::new(cs, Level::High);
    let busy = Input::new(busy, Pull::Up);

    let _led = Output::new(led, Level::Low);
    let _btn_up = Input::new(btn_up, Pull::Up);
    let _btn_down = Input::new(btn_down, Pull::Up);
    let _btn_a = Input::new(btn_a, Pull::Up);
    let _btn_b = Input::new(btn_b, Pull::Up);
    let _btn_c = Input::new(btn_c, Pull::Up);

    let spi = Spi::new(
        p.SPI0,
        clk,
        mosi,
        miso,
        p.DMA_CH1,
        p.DMA_CH2,
        spi::Config::default(),
    );
    let spi_bus: Mutex<NoopRawMutex, _> = Mutex::new(spi);
    let spi_dev = SpiDevice::new(&spi_bus, cs);
    let mut display = Uc8151::new(spi_dev, dc, busy, reset, Delay);
    display.reset().await;

    let sda = p.PIN_4;
    let scl = p.PIN_5;
    let config = embassy_rp::i2c::Config::default();
    let bus = embassy_rp::i2c::I2c::new_async(p.I2C0, scl, sda, Irqs, config);

    // set up the PCF8563 device
    let mut rtc = pcf85063a::PCF85063::new(bus);

    match rtc.reset().await {
        Ok(_) => info!("RTC reset successfully"),
        Err(e) => warn!("RTC reset failed: {:?}", e),
    }

    // Initialise display. Using the default LUT speed setting
    let _ = display.setup(LUT::Internal).await;

    let _ = display.update().await;

    let background = BinaryColor::On;
    let foreground = BinaryColor::Off;

    match spawner.spawn(wifi_task(control, stack)) {
        Ok(_) => {}
        Err(err) => error!("Failed to spawn wifi task: {}", err),
    }

    match spawner.spawn(ntp_task(stack)) {
        Ok(_) => {}
        Err(err) => error!("Failed to spawn ntp task: {}", err),
    }

    let mut wifi_subscriber = WIFI_CHANNEL.subscriber().unwrap();

    let big_numbers =
        u8g2_fonts::FontRenderer::new::<u8g2_font_logisoso38_tn>();
    
    let small_text = u8g2_fonts::FontRenderer::new::<u8g2_fonts::fonts::u8g2_font_pixzillav1_te>();

    loop {
        match select(
            wifi_subscriber.next_message_pure(),
            NTP_TIME_CHANNEL.receive(),
        )
        .await
        {
            Either::First(wifi_state) => {

                let _ = display.setup(LUT::Fast).await;
                let bounds = Rectangle::new(Point::new(0, 0), Size::new(WIDTH, 16));

                bounds
                    .into_styled(
                        PrimitiveStyleBuilder::default()
                            .stroke_color(foreground)
                            .fill_color(background)
                            .stroke_width(2)
                            .build(),
                    )
                    .draw(&mut display)
                    .unwrap();



                let x = (WIDTH / 2) as i32;
                let y = 14;

                let _ = match small_text
                    .render_aligned(
                        wifi_state.to_str(),
                        Point::new(x, y),
                        VerticalPosition::Baseline,
                        HorizontalAlignment::Center,
                        FontColor::Transparent(foreground),
                        &mut display,
                    ) {
                    Ok(bounds) => bounds,
                    Err(_) => {
                        warn!("Failed to render time");
                        None
                    }
                };

                let _ = display.partial_update(bounds.try_into().unwrap()).await;
            }
            Either::Second(ntp_now) => {
                match unix_to_primitive_datetime(ntp_now) {
                    Some(dt) => match rtc.set_datetime(&dt).await {
                        Ok(_) => info!("RTC updated"),
                        Err(e) => warn!("RTC set failed: {:?}", e),
                    },
                    None => warn!("NTP: invalid timestamp"),
                }

                let _ = display.setup(LUT::Fast).await;

                let datetime = ntp_now.to_zoned(TIMEZONE);

                info!("NTP time: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    datetime.year(),
                    datetime.month(),
                    datetime.day(),
                    datetime.hour(),
                    datetime.minute(),
                    datetime.second(),
                );

                let time_str = uformat!(
                    24,
                    "{:02}:{:02}:{:02}",
                    datetime.hour(),
                    datetime.minute(),
                    datetime.second(),
                ).unwrap();


                let date_str = uformat!(
                    24,
                    "{:04}-{:02}-{:02}",
                    datetime.year(),
                    datetime.month(),
                    datetime.day(),
                ).unwrap();

                let x = (WIDTH / 2) as i32;
                let y = 16 + 45;

                let bounds_1 = match big_numbers
                    .render_aligned(
                        date_str.as_ref(),
                        Point::new(x, y),
                        VerticalPosition::Baseline,
                        HorizontalAlignment::Center,
                        FontColor::Transparent(foreground),
                        &mut display,
                    ) {
                    Ok(bounds) => bounds,
                    Err(_) => {
                        warn!("Failed to render time");
                        None
                    }
                };

                let y = y + 45;

                let bounds_2 = match big_numbers
                    .render_aligned(
                        time_str.as_ref(),
                        Point::new(x, y),
                        VerticalPosition::Baseline,
                        HorizontalAlignment::Center,
                        FontColor::Transparent(foreground),
                        &mut display,
                    ) {
                    Ok(bounds) => bounds,
                    Err(_) => {
                        warn!("Failed to render time");
                        None
                    }
                };

                let bounds = match (bounds_1, bounds_2) {
                    (Some(b1), Some(b2)) => {
                        let x = core::cmp::min(b1.top_left.x, b2.top_left.x);
                        let y = core::cmp::min(b1.top_left.y, b2.top_left.y);
                        let right = core::cmp::max(b1.size.width + (b1.top_left.x as u32), b2.size.width + (b2.top_left.x as u32));
                        let bottom = core::cmp::max(b1.size.height + (b1.top_left.y as u32), b2.size.height + (b2.top_left.y as u32));
                        Some(Rectangle::new(Point::new(x, y), Size::new(right - x as u32, bottom - y as u32)))
                    }, // Combine the bounds of both renders
                    (Some(b), None) | (None, Some(b)) => Some(b),
                    (None, None) => {
                        warn!("Failed to render time");
                        None
                    }
                };

                if let Some(bounds) = bounds {
                    info!("Time updated, redrawing display with bounds {} {} {} {}", bounds.top_left.x, bounds.top_left.y, bounds.size.width, bounds.size.height);
                    let y = (bounds.top_left.y / 8) * 8; // Align to 8-pixel boundary
                    let ydiff = (bounds.top_left.y - y) as u32;
                    let height = ((bounds.size.height + 7 + ydiff) / 8) * 8; // Round up to next multiple of 8
                    let bounds = Rectangle::new(
                        Point::new(bounds.top_left.x, y),
                        Size::new(bounds.size.width, height),
                    );
                    info!("Time updated, redrawing display with bounds {} {} {} {}", bounds.top_left.x, bounds.top_left.y, bounds.size.width, bounds.size.height);
                    let _ = display.partial_update(bounds.try_into().unwrap()).await;
                }
            }
        }
    }
}
