//! This example shows generating audio and sending it to a connected i2s DAC using the PIO
//! module of the RP235x.
//!
//! Connect the i2s DAC as follows:
//!   bclk : GPIO 18
//!   lrc  : GPIO 19
//!   din  : GPIO 20
//! Then ground GPIO 0 to trigger a rising triangle waveform.

#![no_std]
#![no_main]

use core::mem;

use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Pull};
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{InterruptHandler, Pio, program};

use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_rp::Peri;
use embassy_rp::dma::{AnyChannel, Channel, Transfer};
use embassy_rp::gpio::Drive;
use embassy_rp::pio::{
    Common, Config, Direction, FifoJoin, Instance, LoadedProgram, PioPin, ShiftConfig,
    ShiftDirection, StateMachine,
};

use fixed::traits::ToFixed;

/// Represents an I2S output driver program.
pub struct PioI2sOutProgram<'d, PIO: Instance> {
    prg: LoadedProgram<'d, PIO>,
}

impl<'d, PIO: Instance> PioI2sOutProgram<'d, PIO> {
    /// Loads the program into the given PIO.
    pub fn new(common: &mut Common<'d, PIO>) -> Self {
        let prg = program::pio_asm!(
            ".side_set 2",                      // side 0bWB - W = Word Clock, B = Bit Clock
            "    mov x, y           side 0b01", // y stores sample depth - 2 (14 = 16bit, 22 = 24bit, 30 = 32bit)
            "left_data:",
            "    out pins, 1        side 0b00",
            "    jmp x-- left_data  side 0b01",
            "    out pins, 1        side 0b10",
            "    mov x, y           side 0b11",
            "right_data:",
            "    out pins, 1         side 0b10",
            "    jmp x-- right_data  side 0b11",
            "    out pins, 1         side 0b00",
        );

        let prg = common.load_program(&prg.program);

        Self { prg }
    }
}

/// PIO backed I2S output driver.
pub struct PioI2sOut<'d, P: Instance, const S: usize> {
    dma: Peri<'d, AnyChannel>,
    sm: StateMachine<'d, P, S>,
}

impl<'d, P: Instance, const S: usize> PioI2sOut<'d, P, S> {
    /// Configures a state machine to output I2S.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        common: &mut Common<'d, P>,
        mut sm: StateMachine<'d, P, S>,
        dma: Peri<'d, impl Channel>,
        data_pin: Peri<'d, impl PioPin>,
        bit_clock_pin: Peri<'d, impl PioPin>,
        lr_clock_pin: Peri<'d, impl PioPin>,
        sample_rate: u32,
        bit_depth: u32,
        channels: u32,
        program: &PioI2sOutProgram<'d, P>,
    ) -> Self {
        let mut data_pin = common.make_pio_pin(data_pin);
        let mut bit_clock_pin = common.make_pio_pin(bit_clock_pin);
        let mut left_right_clock_pin = common.make_pio_pin(lr_clock_pin);

        // Lower the drive strength of the pins.
        data_pin.set_drive_strength(Drive::_2mA);
        bit_clock_pin.set_drive_strength(Drive::_2mA);
        left_right_clock_pin.set_drive_strength(Drive::_2mA);

        let cfg = {
            let mut cfg = Config::default();
            cfg.use_program(&program.prg, &[&bit_clock_pin, &left_right_clock_pin]);
            cfg.set_out_pins(&[&data_pin]);
            let clock_frequency = sample_rate * bit_depth * channels;
            cfg.clock_divider =
                (embassy_rp::clocks::clk_sys_freq() as f32 / clock_frequency as f32 / 2.0)
                    .to_fixed();
            cfg.shift_out = ShiftConfig {
                threshold: 32,
                direction: ShiftDirection::Left,
                auto_fill: true,
            };

            // Join FIFOs to have twice the time to start the next DMA transfer.
            cfg.fifo_join = FifoJoin::TxOnly;

            cfg
        };

        sm.set_config(&cfg);
        sm.set_pin_dirs(
            Direction::Out,
            &[&data_pin, &left_right_clock_pin, &bit_clock_pin],
        );

        // Set the `y` register up to configure the sample depth.
        // The SM counts down to 0 and uses one clock cycle to set up the counter,
        // which results in bit_depth - 2 as register value.
        unsafe { sm.set_y(bit_depth - 2) };

        sm.set_enable(true);

        Self {
            dma: dma.into(),
            sm,
        }
    }

    /// Returns a DMA transfer future. Awaiting it will guarantee a complete transfer.
    pub fn write<'b>(&'b mut self, buffer: &'b [u32]) -> Transfer<'b, AnyChannel> {
        self.sm.tx().dma_push(self.dma.reborrow(), buffer, false)
    }
}

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

const SAMPLE_RATE: u32 = 48_000;
const BIT_DEPTH: u32 = 16;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Setup pio state machine for i2s output
    let Pio { mut common, sm0, .. } = Pio::new(p.PIO0, Irqs);

    let tx_dma = p.DMA_CH0;
    let bclk_pin = p.PIN_18;
    let lrclk_pin = p.PIN_19;
    let dout_pin = p.PIN_20;
    let channels = 2;

    let program = PioI2sOutProgram::new(&mut common);
    
    let mut i2s = PioI2sOut::new(
        &mut common,
        sm0,
        tx_dma,
        dout_pin,
        bclk_pin,
        lrclk_pin,
        SAMPLE_RATE,
        BIT_DEPTH,
        channels,
        &program,
    );


    let fade_input = Input::new(p.PIN_0, Pull::Up);

    // create two audio buffers (back and front) which will take turns being
    // filled with new audio data and being sent to the pio fifo using dma
    const BUFFER_SIZE: usize = 960;
    static DMA_BUFFER: StaticCell<[u32; BUFFER_SIZE * 2]> = StaticCell::new();
    let dma_buffer = DMA_BUFFER.init_with(|| [0u32; BUFFER_SIZE * 2]);
    let (mut back_buffer, mut front_buffer) = dma_buffer.split_at_mut(BUFFER_SIZE);

    // start pio state machine
    let mut fade_value: i32 = 0;
    let mut phase: i32 = 0;

    loop {
        // trigger transfer of front buffer data to the pio fifo
        // but don't await the returned future, yet
        let dma_future = i2s.write(front_buffer);

        // fade in audio when gpio0 is pulled low
        let fade_target = if fade_input.is_low() { i32::MAX } else { 0 };

        // fill back buffer with fresh audio samples before awaiting the dma future
        for s in back_buffer.iter_mut() {
            // exponential approach of fade_value => fade_target
            fade_value += (fade_target - fade_value) >> 14;
            // generate triangle wave with amplitude and frequency based on fade value
            phase = (phase + (fade_value >> 22)) & 0xffff;
            let triangle_sample = (phase as i16 as i32).abs() - 16384;
            let sample = (triangle_sample * (fade_value >> 15)) >> 16;
            // duplicate mono sample into lower and upper half of dma word
            *s = (sample as u16 as u32) * 0x10001;
        }

        // now await the dma future. once the dma finishes, the next buffer needs to be queued
        // within DMA_DEPTH / SAMPLE_RATE = 8 / 48000 seconds = 166us
        dma_future.await;
        mem::swap(&mut back_buffer, &mut front_buffer);
    }
}
