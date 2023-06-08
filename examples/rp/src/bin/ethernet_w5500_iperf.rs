//! basic iperf tcp server
//!
//! use iperf3 -c [pico ip address]
//! add --reverse for send instead of receive
//! add --bidirectional for send and receieve
//!
//! Example written for the [`WIZnet W5500-EVB-Pico`](https://www.wiznet.io/product-item/w5500-evb-pico/) board.

#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::yield_now;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_net_w5500::*;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIN_17, PIN_20, PIN_21, SPI0};
use embassy_rp::spi::{Async, Config as SpiConfig, Spi};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use embedded_hal_async::spi::ExclusiveDevice;
use embedded_io::asynch::{Read, Write};
use heapless::Vec;
use rand::RngCore;
use static_cell::make_static;
use {defmt_rtt as _, panic_probe as _};

#[embassy_executor::task]
async fn ethernet_task(
    runner: Runner<
        'static,
        ExclusiveDevice<Spi<'static, SPI0, Async>, Output<'static, PIN_17>>,
        Input<'static, PIN_21>,
        Output<'static, PIN_20>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<Device<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;

    let mut spi_cfg = SpiConfig::default();
    spi_cfg.frequency = 50_000_000;
    let (miso, mosi, clk) = (p.PIN_16, p.PIN_19, p.PIN_18);
    let spi = Spi::new(p.SPI0, clk, mosi, miso, p.DMA_CH0, p.DMA_CH1, spi_cfg);
    let cs = Output::new(p.PIN_17, Level::High);
    let w5500_int = Input::new(p.PIN_21, Pull::Up);
    let w5500_reset = Output::new(p.PIN_20, Level::High);

    let mac_addr = [0x02, 0x00, 0x00, 0x00, 0x00, 0x00];
    let state = make_static!(State::<8, 8>::new());
    let (device, runner) =
        embassy_net_w5500::new(mac_addr, state, ExclusiveDevice::new(spi, cs), w5500_int, w5500_reset).await;
    unwrap!(spawner.spawn(ethernet_task(runner)));

    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    let stack = &*make_static!(Stack::new(
        device,
        embassy_net::Config::Dhcp(Default::default()),
        make_static!(StackResources::<4>::new()),
        seed
    ));

    // Launch network task
    unwrap!(spawner.spawn(net_task(stack)));

    info!("Waiting for DHCP...");
    let cfg = wait_for_config(stack).await;
    let local_addr = cfg.address.address();
    info!("IP address: {:?}", local_addr);

    // Create two sockets listening to the same port, to handle simultaneous connections
    unwrap!(spawner.spawn(listen_task(stack, 0, 5201)));
    unwrap!(spawner.spawn(listen_task(stack, 1, 5201)));
    unwrap!(spawner.spawn(listen_task(stack, 2, 5201)));
}

#[embassy_executor::task(pool_size = 3)]
async fn listen_task(stack: &'static Stack<Device<'static>>, id: u8, port: u16) {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        info!("SOCKET {}: Listening on TCP:{}...", id, port);
        if let Err(e) = socket.accept(port).await {
            warn!("accept error: {:?}", e);
            continue;
        }
        info!("SOCKET {}: Received connection from {:?}", id, socket.remote_endpoint());

        match process(socket).await {
            Ok(_) => warn!("Closing SOCKET {}: session complete", id),
            Err(e) => warn!("Closing SOCKET {}: error {:?}", id, e),
        }
    }
}

async fn wait_for_config(stack: &'static Stack<Device<'static>>) -> embassy_net::StaticConfig {
    loop {
        if let Some(config) = stack.config() {
            return config.clone();
        }
        yield_now().await;
    }
}

const TEST_START: u8 = 1;
const TEST_RUNNING: u8 = 2;
const TEST_END: u8 = 4;
const PARAM_EXCHANGE: u8 = 9;
const CREATE_STREAMS: u8 = 10;
const DISPLAY_RESULTS: u8 = 14;
const IPERF_DONE: u8 = 16;

pub enum ConnectionType {
    Control,
    Data,
}

#[derive(Debug, PartialEq)]
pub enum DataStreamType {
    Sender,
    Receiver,
    None,
}

#[derive(Debug)]
struct Session {
    cookie: [u8; 36],
    num_senders: u8,
    num_receivers: u8,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.cookie == other.cookie
    }
}

static mut SESSIONS: Mutex<CriticalSectionRawMutex, Vec<Session, 2>> = Mutex::new(Vec::new());

async fn process(mut socket: TcpSocket<'_>) -> core::result::Result<(), ()> {
    info!("Received connection from {:?}", socket.remote_endpoint());
    socket.set_timeout(Some(Duration::from_secs(20)));
    let (mut buf_reader, mut buf_writer) = socket.split();

    let mut session_id: [u8; 36] = [0; 36];

    {
        let mut session_id_cstr: [u8; 37] = [0; 37];
        buf_reader.read_exact(&mut session_id_cstr).await.unwrap();
        if !session_id_cstr.is_ascii() {
            info!("Not a valid iperf session");
            return Err(());
        }
        // The string is a C style nul terminated thing, so strip that off.
        session_id.copy_from_slice(&session_id_cstr[..36]);
    }
    info!("session_id: {}", core::str::from_utf8(&session_id).unwrap());

    let connection_type = {
        let existing_session = unsafe {
            SESSIONS.lock().await.contains(&Session {
                cookie: session_id,
                num_senders: 0,
                num_receivers: 0,
            })
        };
        if existing_session {
            ConnectionType::Data
        } else {
            ConnectionType::Control
        }
    };
    match connection_type {
        ConnectionType::Control => {
            info!("processing first request");
            buf_writer.write_all(&[PARAM_EXCHANGE]).await.unwrap();
            // buf_writer.flush().await.unwrap();
            let mut message_len: [u8; 4] = [0; 4];
            buf_reader.read_exact(&mut message_len).await.unwrap();
            info!("message len arr: {}", message_len);

            let message_len = u32::from_be_bytes(message_len);
            info!("message len: {}", message_len);

            if message_len > 1024 {
                defmt::panic!("Too much JSON!");
            }
            static mut JSON_BUFFER: [u8; 1024] = [0; 1024];
            let buf = unsafe { &mut JSON_BUFFER[0..message_len as usize] };

            buf_reader.read_exact(buf).await.unwrap();
            if buf.is_ascii() {
                info!("buf: {}", core::str::from_utf8(buf).unwrap());
            }

            // this is slow but I don't mind :)
            fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
                haystack.windows(needle.len()).position(|window| window == needle)
            }

            // Don't want to parse json here, but we can extract some useful info anyway
            // The official client doesn't put in bidirection or reverse unless they are true
            let reverse = find_subsequence(buf, b"reverse").is_some();
            let bidir = find_subsequence(buf, b"bidirectional").is_some();
            let (senders, receivers) = if bidir {
                (1, 1)
            } else if reverse {
                (1, 0)
            } else {
                (0, 1)
            };
            unsafe {
                SESSIONS
                    .lock()
                    .await
                    .push(Session {
                        cookie: session_id,
                        num_senders: senders,
                        num_receivers: receivers,
                    })
                    .unwrap()
            };

            debug!("ask the client to create any data connections");
            buf_writer
                .write_all(&[CREATE_STREAMS])
                .await
                .expect("Failed to send CREATE_STREAMS cmd");
            Timer::after(Duration::from_secs(1)).await;

            debug!("ask the client to start the test");
            buf_writer
                .write_all(&[TEST_START])
                .await
                .expect("Failed to send TEST_START cmd");

            // should probably wait for some data on the other channel for this
            debug!("tell the client we've started running the test");
            buf_writer
                .write_all(&[TEST_RUNNING])
                .await
                .expect("Failed to send TEST_RUNNING cmd");

            // the client should eventually reply with a command
            let mut reply: [u8; 1] = [0; 1];
            buf_reader
                .read_exact(&mut reply)
                .await
                .expect("Did not recv TEST_END as expected");

            // We're hoping that was TEST_END. check:
            if reply[0] == TEST_END {
                debug!("TEST_END command received from client");
                //EXCHANGE_RESULTS - we would need to record some first!
                buf_writer.write_all(&[DISPLAY_RESULTS]).await.unwrap();
            } else {
                debug!("were expecting TEST_END, got {}", reply[0]);
            }

            // Should be done now, check:
            let mut reply: [u8; 1] = [0; 1];
            let last = buf_reader.read_exact(&mut reply).await;
            match last {
                Ok(_) => {
                    if reply[0] == IPERF_DONE {
                        debug!("IPERF_DONE received from client.");
                    } else {
                        debug!("were expecting IPERF_DONE, got {}", reply[0]);
                    }
                }
                Err(_) => {
                    debug!("no final message, strange.");
                }
            }
        }
        ConnectionType::Data => {
            let streamtype = unsafe {
                let mut sessiontype = DataStreamType::None;
                for session in &mut *SESSIONS.lock().await {
                    if session.cookie == session_id {
                        if session.num_receivers == 1 {
                            session.num_receivers = 0;
                            sessiontype = DataStreamType::Receiver;
                        } else if session.num_senders == 1 {
                            session.num_senders = 0;
                            sessiontype = DataStreamType::Sender;
                        }
                    }
                    if sessiontype != DataStreamType::None {
                        break;
                    }
                }
                sessiontype
            };
            // let mut message: [u8; 0x1000] = [0; 0x1000];
            let mut message: [u8; 1024] = [0; 1024];
            match streamtype {
                DataStreamType::Sender => {
                    // Reverse mode - send data to client
                    let mut bytes_total: u64 = 0;
                    let mut done = false;
                    while !done {
                        match buf_writer.write(&message).await {
                            Ok(sz) => bytes_total += sz as u64,
                            Err(_) => done = true,
                        }
                    }
                    let gb_total = bytes_total as f32 / (1024f32 * 1024f32 * 1024f32);
                    let gbit_sec = gb_total * 8f32 / 10f32;
                    info!(
                        "we're done sending. sent {} bytes ({}GB), {} GBits/sec",
                        bytes_total, gb_total, gbit_sec
                    );
                }
                DataStreamType::Receiver => {
                    // Forward mode - receive data from client
                    let mut bytes_total: u64 = 0;
                    let mut done = false;
                    while !done {
                        let sz = buf_reader.read(&mut message).await.expect("good");
                        if sz == 0 {
                            done = true;
                        }
                        bytes_total += sz as u64;
                    }
                    let gb_total = bytes_total as f32 / (1024f32 * 1024f32 * 1024f32);
                    let gbit_sec = gb_total * 8f32 / 10f32;
                    info!(
                        "we're done receiving. received {} bytes ({}GB), {} GBits/sec",
                        bytes_total, gb_total, gbit_sec
                    );
                }
                DataStreamType::None => {
                    //
                    info!("Invalid mode, handle this!");
                }
            }
        }
    }

    Ok(())
}
