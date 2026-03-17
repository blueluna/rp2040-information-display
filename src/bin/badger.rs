#![no_std]
#![no_main]

use cyw43_pio::{DEFAULT_CLOCK_DIVIDER, PioSpi};
use defmt::*;
use efmt::uformat;
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_executor::Spawner;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::spi::{self, Spi};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Delay, Duration, Ticker};
use embedded_alloc::Heap;
use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyleBuilder, Rectangle},
};
use pcf85063a::Control as RtcAlarmControl;
use static_cell::StaticCell;
use u8g2_fonts::fonts::{u8g2_font_logisoso20_tf, u8g2_font_logisoso38_tf};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use uc8151::asynch::Uc8151;
use uc8151::{HEIGHT, LUT, WIDTH};

use rp2040_badger_w::{draw, mqtt, ntp, time_util, wifi};

use {defmt_rtt as _, panic_probe as _};

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    I2C0_IRQ => embassy_rp::i2c::InterruptHandler<embassy_rp::peripherals::I2C0>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    {
        static mut HEAP_MEM: [u8; 8192] = [0u8; 8192];
        unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, 8192) }
    }

    info!("Starting");

    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;

    let fw = include_bytes!("../../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../../cyw43-firmware/43439A0_clm.bin");

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
    match spawner.spawn(wifi::cyw43_task(runner)) {
        Ok(_) => {}
        Err(err) => error!("Failed to spawn cyw43 task: {}", err),
    }

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());
    let seed = rng.next_u64();

    static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();
    let (stack, runner) =
        embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);
    match spawner.spawn(wifi::net_task(runner)) {
        Ok(_) => {}
        Err(err) => error!("Failed to spawn net task: {}", err),
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

    // PCF85063A INT pin — open-drain active-low; pulled high by RP2040,
    // driven low by RTC when an alarm fires.
    let mut rtc_int = Input::new(p.PIN_8, Pull::Down);

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

    let mut rtc = pcf85063a::PCF85063::new(bus);

    // Arm the once-per-minute alarm immediately so the display ticks from
    // whatever time the RTC already holds (retained across reboots).
    let _ = rtc.disable_all_alarms().await;
    let _ = rtc.set_alarm_seconds(0).await;
    let _ = rtc.control_alarm_seconds(RtcAlarmControl::On).await;
    let _ = rtc.control_alarm_interrupt(RtcAlarmControl::On).await;
    let _ = rtc.clear_alarm_flag().await;
    info!("RTC alarm armed");

    let _ = display.setup(LUT::Internal).await;
    let _ = display.update().await;

    let background = BinaryColor::On;
    let foreground = BinaryColor::Off;

    match spawner.spawn(wifi::wifi_task(control, stack)) {
        Ok(_) => {}
        Err(err) => error!("Failed to spawn wifi task: {}", err),
    }

    match spawner.spawn(ntp::ntp_task(stack)) {
        Ok(_) => {}
        Err(err) => error!("Failed to spawn ntp task: {}", err),
    }

    match spawner.spawn(mqtt::mqtt_task(stack)) {
        Ok(_) => {}
        Err(err) => error!("Failed to spawn mqtt task: {}", err),
    }

    let mut wifi_subscriber = wifi::WIFI_CHANNEL.subscriber().unwrap();

    let big_numbers = u8g2_fonts::FontRenderer::new::<u8g2_font_logisoso38_tf>();
    let date_numbers = u8g2_fonts::FontRenderer::new::<u8g2_font_logisoso20_tf>();
    let small_text =
        u8g2_fonts::FontRenderer::new::<u8g2_fonts::fonts::u8g2_font_pixzillav1_te>();

    // Last displayed hour; None forces a full refresh on the first tick.
    let mut last_hour: Option<i8> = None;
    // Tracked so it can be redrawn on the hourly full refresh.
    let mut last_wifi_state = wifi::WifiState::Disconnected;
    // Last received temperatures, redrawn on full refreshes.
    let mut last_temps: Option<(f32, f32)> = None;

    let mut watchdog_ticker = Ticker::every(Duration::from_secs(600));

    loop {
        match select(
            select4(
                wifi_subscriber.next_message_pure(),
                ntp::NTP_TIME_CHANNEL.receive(),
                rtc_int.wait_for_any_edge(),
                mqtt::TEMPERATURE_CHANNEL.receive(),
            ),
            watchdog_ticker.next(),
        )
        .await
        {
            // ── Wi-Fi state change ────────────────────────────────────────────
            Either::First(Either4::First(wifi_state)) => {
                last_wifi_state = wifi_state;

                let status_area =
                    Rectangle::new(Point::new(0, 0), Size::new(WIDTH, draw::STATUS_HEIGHT));
                status_area
                    .into_styled(
                        PrimitiveStyleBuilder::default()
                            .fill_color(background)
                            .build(),
                    )
                    .draw(&mut display)
                    .unwrap();
                let _ = small_text.render_aligned(
                    wifi_state.to_str(),
                    Point::new((WIDTH / 2) as i32, draw::STATUS_HEIGHT as i32 - 2),
                    VerticalPosition::Baseline,
                    HorizontalAlignment::Center,
                    FontColor::Transparent(foreground),
                    &mut display,
                );
                let _ = display.setup(LUT::Fast).await;
                let _ = display.partial_update(status_area.try_into().unwrap()).await;
            }

            // ── NTP time received ─────────────────────────────────────────────
            Either::First(Either4::Second(ntp_now)) => {
                // Sync the RTC.
                if let Some(dt) = time_util::unix_to_primitive_datetime(ntp_now) {
                    info!(
                        "Setting RTC to {:04}-{:02}-{:02} {:02}:{:02}:{:02} (UTC)",
                        dt.year(),
                        dt.month() as u8,
                        dt.day(),
                        dt.hour(),
                        dt.minute(),
                        dt.second()
                    );
                    match rtc.set_datetime(&dt).await {
                        Ok(_) => {
                            info!("RTC updated");
                            let _ = rtc.set_alarm_seconds(0).await;
                            let _ = rtc.control_alarm_seconds(RtcAlarmControl::On).await;
                            let _ = rtc.control_alarm_interrupt(RtcAlarmControl::On).await;
                            let _ = rtc.clear_alarm_flag().await;
                            info!("RTC alarm re-armed");
                        }
                        Err(e) => warn!("RTC set failed: {:?}", e),
                    }
                }

                // Immediately render the current time (full refresh).
                let local = ntp_now.to_zoned(time_util::TIMEZONE);
                let hour = local.hour();
                let minute = local.minute();
                last_hour = Some(hour);

                info!(
                    "NTP time: {:04}-{:02}-{:02} {:02}:{:02} (local)",
                    local.year(),
                    local.month(),
                    local.day(),
                    hour,
                    minute,
                );

                let date_str =
                    uformat!(11, "{:04}-{:02}-{:02}", local.year(), local.month(), local.day())
                        .unwrap();
                let time_str = uformat!(6, "{:02}:{:02}", hour, minute).unwrap();

                Rectangle::new(Point::new(0, 0), Size::new(WIDTH, HEIGHT))
                    .into_styled(
                        PrimitiveStyleBuilder::default()
                            .fill_color(background)
                            .build(),
                    )
                    .draw(&mut display)
                    .unwrap();
                let _ = small_text.render_aligned(
                    last_wifi_state.to_str(),
                    Point::new((WIDTH / 2) as i32, draw::STATUS_HEIGHT as i32 - 2),
                    VerticalPosition::Baseline,
                    HorizontalAlignment::Center,
                    FontColor::Transparent(foreground),
                    &mut display,
                );
                // Date — left column, top row.
                let _ = date_numbers.render_aligned(
                    date_str.as_ref(),
                    Point::new(draw::LEFT_COL_CENTER_X, draw::DATE_BASELINE_Y),
                    VerticalPosition::Baseline,
                    HorizontalAlignment::Center,
                    FontColor::Transparent(foreground),
                    &mut display,
                );
                // Time — left column, bottom row.
                let _ = big_numbers.render_aligned(
                    time_str.as_ref(),
                    Point::new(draw::LEFT_COL_CENTER_X, draw::TIME_BASELINE_Y),
                    VerticalPosition::Baseline,
                    HorizontalAlignment::Center,
                    FontColor::Transparent(foreground),
                    &mut display,
                );
                if let Some((north, south)) = last_temps {
                    draw::render_temps(north, south, foreground, background, &big_numbers, &mut display);
                }
                let _ = display.setup(LUT::Internal).await;
                let _ = display.update().await;
            }

            // ── RTC INT rising edge — alarm fired ─────────────────────────────
            Either::First(Either4::Third(_)) => {
                if rtc_int.is_low() {
                    info!("RTC INT low");
                }
                else {
                    info!("RTC INT high");
                }
                watchdog_ticker.reset();
                match rtc.get_datetime().await {
                    Err(e) => {
                        warn!("RTC read failed: {:?}", e);
                        let _ = rtc.clear_alarm_flag().await;
                    }
                    Ok(rtc_dt) => {
                        // Clear the alarm flag; this de-asserts INT (pin returns high).
                        let _ = rtc.clear_alarm_flag().await;

                        let Some(ts) = time_util::rtc_to_jiff(rtc_dt) else {
                            continue;
                        };
                        let local = ts.to_zoned(time_util::TIMEZONE);
                        let hour = local.hour();
                        let minute = local.minute();
                        let full_update = last_hour != Some(hour);
                        last_hour = Some(hour);

                        info!("Tick {:02}:{:02} full={}", hour, minute, full_update);

                        let date_str = uformat!(
                            11,
                            "{:04}-{:02}-{:02}",
                            local.year(),
                            local.month(),
                            local.day(),
                        )
                        .unwrap();
                        let time_str = uformat!(6, "{:02}:{:02}", hour, minute).unwrap();

                        if full_update {
                            // Full refresh once per hour: clear screen and redraw everything.
                            Rectangle::new(Point::new(0, 0), Size::new(WIDTH, HEIGHT))
                                .into_styled(
                                    PrimitiveStyleBuilder::default()
                                        .fill_color(background)
                                        .build(),
                                )
                                .draw(&mut display)
                                .unwrap();
                            let _ = small_text.render_aligned(
                                last_wifi_state.to_str(),
                                Point::new((WIDTH / 2) as i32, draw::STATUS_HEIGHT as i32 - 2),
                                VerticalPosition::Baseline,
                                HorizontalAlignment::Center,
                                FontColor::Transparent(foreground),
                                &mut display,
                            );
                            // Date — left column, top row.
                            let _ = date_numbers.render_aligned(
                                date_str.as_ref(),
                                Point::new(draw::LEFT_COL_CENTER_X, draw::DATE_BASELINE_Y),
                                VerticalPosition::Baseline,
                                HorizontalAlignment::Center,
                                FontColor::Transparent(foreground),
                                &mut display,
                            );
                            // Time — left column, bottom row.
                            let _ = big_numbers.render_aligned(
                                time_str.as_ref(),
                                Point::new(draw::LEFT_COL_CENTER_X, draw::TIME_BASELINE_Y),
                                VerticalPosition::Baseline,
                                HorizontalAlignment::Center,
                                FontColor::Transparent(foreground),
                                &mut display,
                            );
                            if let Some((north, south)) = last_temps {
                                draw::render_temps(north, south, foreground, background, &big_numbers, &mut display);
                            }
                            let _ = display.setup(LUT::Internal).await;
                            let _ = display.update().await;
                        } else {
                            // Partial refresh — update the left bottom quadrant (time area).
                            let time_area = Rectangle::new(
                                Point::new(0, draw::CONTENT_MID_Y),
                                Size::new(
                                    draw::RIGHT_COL_X as u32,
                                    (HEIGHT as i32 - draw::CONTENT_MID_Y) as u32,
                                ),
                            );
                            time_area
                                .into_styled(
                                    PrimitiveStyleBuilder::default()
                                        .fill_color(background)
                                        .build(),
                                )
                                .draw(&mut display)
                                .unwrap();
                            let _ = big_numbers.render_aligned(
                                time_str.as_ref(),
                                Point::new(draw::LEFT_COL_CENTER_X, draw::TIME_BASELINE_Y),
                                VerticalPosition::Baseline,
                                HorizontalAlignment::Center,
                                FontColor::Transparent(foreground),
                                &mut display,
                            );
                            let _ = display.setup(LUT::Fast).await;
                            let _ = display
                                .partial_update(time_area.try_into().unwrap())
                                .await;
                        }
                    }
                }
            }

            // ── MQTT temperature update ───────────────────────────────────────
            Either::First(Either4::Fourth((north, south))) => {
                if last_temps == Some((north, south)) {
                    continue;
                }
                last_temps = Some((north, south));

                // Partial-update the right column only.
                let temp_area = Rectangle::new(
                    Point::new(draw::RIGHT_COL_X, draw::STATUS_HEIGHT as i32),
                    Size::new(draw::RIGHT_COL_W, HEIGHT - draw::STATUS_HEIGHT),
                );
                draw::render_temps(north, south, foreground, background, &big_numbers, &mut display);
                let _ = display.setup(LUT::Fast).await;
                let _ = display.partial_update(temp_area.try_into().unwrap()).await;
            }

            // ── Watchdog — RTC INT may be stuck ──────────────────────────────
            Either::Second(_) => {
                warn!("RTC watchdog tick — INT line may be stuck");
            }
        }
    }
}
