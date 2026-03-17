use efmt::uformat;
use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyleBuilder, Rectangle},
};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use uc8151::HEIGHT;

/// Height of the Wi-Fi status bar at the top of the display.
pub const STATUS_HEIGHT: u32 = 16;
/// Y boundary between the date row (top) and time row (bottom) of the content area.
/// Content area is y=16..128 (112 px); mid = 16 + 56 = 72. (8-px aligned)
pub const CONTENT_MID_Y: i32 = 72;
/// X boundary between the left column (date/time) and right column (temperatures).
/// Nearest 8-px boundary to the display centre (148). (8-px aligned)
pub const RIGHT_COL_X: i32 = 144;
/// Width of the right column: 296 − 144 = 152. (8-px aligned)
pub const RIGHT_COL_W: u32 = 152;
/// Centre X of the left column, used as the text-alignment anchor for date and time.
pub const LEFT_COL_CENTER_X: i32 = 72;
/// Centre X of the right column, used as the text-alignment anchor for temperatures.
pub const RIGHT_COL_CENTER_X: i32 = 220;

// Baselines — font centred vertically in its 56-px row:
//   baseline = row_top + (row_height + ascent) / 2
//   logisoso20 ascent ≈ 20 px  → baseline offset from row_top = (56+20)/2 = 38
//   logisoso38 ascent ≈ 38 px  → baseline offset from row_top = (56+38)/2 = 47

/// Date baseline: logisoso20 in top row (y = 16..72).  16 + 38 = 54.
pub const DATE_BASELINE_Y: i32 = 54;
/// Time baseline: logisoso38 in bottom row (y = 72..128).  72 + 47 = 119.
pub const TIME_BASELINE_Y: i32 = 119;
/// North temperature baseline: logisoso38 in top row (y = 16..72).  16 + 47 = 63.
pub const NORTH_BASELINE_Y: i32 = 63;
/// South temperature baseline: logisoso38 in bottom row (y = 72..128).  72 + 47 = 119.
pub const SOUTH_BASELINE_Y: i32 = 119;

/// Render North (top-right) and South (bottom-right) temperatures in the right column.
/// Clears the right column before drawing.
pub fn render_temps(
    north: f32,
    south: f32,
    foreground: BinaryColor,
    background: BinaryColor,
    renderer: &u8g2_fonts::FontRenderer,
    display: &mut impl DrawTarget<Color = BinaryColor, Error = core::convert::Infallible>,
) {
    Rectangle::new(
        Point::new(RIGHT_COL_X, STATUS_HEIGHT as i32),
        Size::new(RIGHT_COL_W, HEIGHT - STATUS_HEIGHT),
    )
    .into_styled(PrimitiveStyleBuilder::default().fill_color(background).build())
    .draw(display)
    .unwrap();

    let north_str = uformat!(10, "{:5.1}°", north).unwrap();
    let _ = renderer.render_aligned(
        north_str.as_ref(),
        Point::new(RIGHT_COL_CENTER_X, NORTH_BASELINE_Y),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(foreground),
        display,
    );

    let south_str = uformat!(10, "{:5.1}°", south).unwrap();
    let _ = renderer.render_aligned(
        south_str.as_ref(),
        Point::new(RIGHT_COL_CENTER_X, SOUTH_BASELINE_Y),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(foreground),
        display,
    );
}
