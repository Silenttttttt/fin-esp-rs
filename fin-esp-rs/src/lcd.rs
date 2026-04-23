use esp_idf_hal::i2c::I2cDriver;
use esp_idf_hal::delay::FreeRtos;
use std::sync::Mutex;

// PCF8574 backpack bit assignments
const RS: u8 = 0x01;
const EN: u8 = 0x04;
const BACKLIGHT: u8 = 0x08;

// HD44780 commands
const CMD_CLEAR: u8 = 0x01;
const CMD_HOME: u8 = 0x02;
const CMD_ENTRY_MODE: u8 = 0x04;
const CMD_DISPLAY_CTRL: u8 = 0x08;
const CMD_FUNCTION_SET: u8 = 0x20;
const CMD_SET_CGRAM: u8 = 0x40;
const CMD_SET_DDRAM: u8 = 0x80;

// Flags
const ENTRY_LEFT: u8 = 0x02;
const DISPLAY_ON: u8 = 0x04;
const MODE_2LINE: u8 = 0x08;

// DDRAM row offsets for 20x4 LCD
const ROW_OFFSETS: [u8; 4] = [0x00, 0x40, 0x14, 0x54];

pub struct Lcd<'a> {
    i2c: &'a Mutex<I2cDriver<'a>>,
    addr: u8,
    backlight: u8,
}

impl<'a> Lcd<'a> {
    pub fn new(i2c: &'a Mutex<I2cDriver<'a>>, addr: u8) -> Self {
        Self {
            i2c,
            addr,
            backlight: BACKLIGHT,
        }
    }

    pub fn init(&mut self) {
        FreeRtos::delay_ms(50);
        // HD44780 init sequence: switch to 4-bit mode
        self.write4bits(0x03 << 4);
        FreeRtos::delay_ms(5);
        self.write4bits(0x03 << 4);
        FreeRtos::delay_ms(5);
        self.write4bits(0x03 << 4);
        FreeRtos::delay_ms(1);
        self.write4bits(0x02 << 4); // 4-bit mode

        self.command(CMD_FUNCTION_SET | MODE_2LINE); // 4-bit, 2 lines, 5x8 font
        self.command(CMD_DISPLAY_CTRL | DISPLAY_ON); // display on, cursor off
        self.clear();
        self.command(CMD_ENTRY_MODE | ENTRY_LEFT); // left-to-right
    }

    pub fn clear(&mut self) {
        self.command(CMD_CLEAR);
        FreeRtos::delay_ms(2);
    }

    pub fn home(&mut self) {
        self.command(CMD_HOME);
        FreeRtos::delay_ms(2);
    }

    pub fn set_cursor(&mut self, col: u8, row: u8) {
        let row = row.min(3) as usize;
        self.command(CMD_SET_DDRAM | (ROW_OFFSETS[row] + col));
    }

    pub fn print(&mut self, text: &str) {
        for b in text.bytes() {
            self.write_byte(b, true);
        }
    }

    /// Write raw bytes (including CGRAM character codes 0-7).
    pub fn write_raw(&mut self, data: &[u8]) {
        for &b in data {
            self.write_byte(b, true);
        }
    }

    /// Define a custom character at CGRAM slot (0-7).
    pub fn create_char(&mut self, slot: u8, pattern: &[u8; 8]) {
        self.command(CMD_SET_CGRAM | ((slot & 0x07) << 3));
        for &b in pattern {
            self.write_byte(b, true);
        }
    }

    pub fn backlight_on(&mut self) {
        self.backlight = BACKLIGHT;
        self.expand_write(0);
    }

    pub fn backlight_off(&mut self) {
        self.backlight = 0;
        self.expand_write(0);
    }

    /// Set backlight state without any LCD command (just toggles bit 3 of PCF8574).
    /// Safe to call between LCD operations for software PWM dimming.
    pub fn write_backlight(&mut self, on: bool) {
        self.backlight = if on { BACKLIGHT } else { 0 };
        if let Ok(mut i2c) = self.i2c.lock() {
            let _ = i2c.write(self.addr, &[self.backlight], 100);
        }
    }

    fn command(&mut self, cmd: u8) {
        self.write_byte(cmd, false);
    }

    fn write_byte(&mut self, value: u8, rs: bool) {
        let rs_bit = if rs { RS } else { 0 };
        let high = (value & 0xF0) | rs_bit;
        let low = ((value << 4) & 0xF0) | rs_bit;
        self.write4bits(high);
        self.write4bits(low);
    }

    fn write4bits(&mut self, value: u8) {
        self.expand_write(value);
        self.pulse_enable(value);
    }

    fn pulse_enable(&mut self, value: u8) {
        self.expand_write(value | EN);
        // EN pulse >450ns; I2C transaction is already slow enough
        self.expand_write(value & !EN);
    }

    fn expand_write(&mut self, data: u8) {
        let byte = data | self.backlight;
        if let Ok(mut i2c) = self.i2c.lock() {
            let _ = i2c.write(self.addr, &[byte], 100);
        }
    }
}
