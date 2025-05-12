use cdefmt_decoder::var::Var;
use clap::Parser;
use cobs2::cobsr::{decode_array, decode_max_output_size};
use mio::{net::UdpSocket as MioUdpSocket, Events, Interest, Poll, Token};
use mio_serial::{SerialPortBuilderExt, SerialStream};
use std::{
    io::{stdout, ErrorKind, Read, Write},
    net::UdpSocket as StdUdpSocket,
    path::PathBuf,
    time::{Duration, Instant},
};

mod commands;
use commands::process_udp_message;

const SERIAL_TOKEN: Token = Token(0);
const COMMAND_UDP_TOKEN: Token = Token(1);

const MAX_UDP_PACKET_SIZE: usize = 512;

/// CLI args
#[derive(clap::Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    elf: PathBuf,

    #[arg(short, long)]
    port: String,

    #[arg(short, long, default_value_t = 115200)]
    baud_rate: u32,

    #[arg(long, default_value_t = false)]
    status_only: bool,

    #[arg(long, default_value_t = 8888)]
    telemetry_port: u16,

    #[arg(long, default_value_t = 8889)]
    command_port: u16,
}

fn timeplot_data(channel: &str, series: &str, value: u32) -> String {
    format!("{{TIMEPLOT:{}|DATA|{}|T|{:.2}}}\n", channel, series, value)
}

fn xyplot_data(channel: &str, series: &str, x: u64, y: f32) -> String {
    format!("{{XYPLOT:{}|DATA|{}|{}|{:.2}}}\n", channel, series, x, y)
}

const SERIAL_BUF_SIZE: usize = 64;

struct SerialGateway<'a> {
    msg_count: u64,
    dropped_count: u64,
    start_time: Instant,
    last_status_update: Instant,
    port: SerialStream,
    poll: Poll,
    events: Events,
    // Takes ownership of the Decoder value, but Decoder still borrows mmap data
    decoder: cdefmt_decoder::Decoder<'a>,
    status_only: bool,
    telemetry_socket: StdUdpSocket, // For sending plot data
    command_socket: MioUdpSocket,   // For receiving commands
}

impl<'a> SerialGateway<'a> {
    fn new(
        mut port: SerialStream,
        decoder: cdefmt_decoder::Decoder<'a>,
        status_only: bool,
        telemetry_port: u16,
        command_port: u16,
    ) -> Result<Self, String> {
        let now = Instant::now();

        // Telemetry UDP socket (for sending plot data)
        let telemetry_socket = StdUdpSocket::bind("0.0.0.0:0")
            .map_err(|e| format!("Failed to bind telemetry UDP: {}", e))?;
        telemetry_socket
            .connect(format!("127.0.0.1:{}", telemetry_port))
            .map_err(|e| {
                format!(
                    "Failed to connect telemetry UDP to 127.0.0.1:{}: {}",
                    telemetry_port, e
                )
            })?;

        // Command UDP socket (for receiving commands to send to serial)
        let command_std_socket = StdUdpSocket::bind(format!("0.0.0.0:{}", command_port))
            .map_err(|e| format!("Failed to bind command UDP port {}: {}", command_port, e))?;
        command_std_socket
            .set_nonblocking(true)
            .map_err(|e| format!("Failed to set command UDP to non-blocking: {}", e))?;
        let mut command_socket = MioUdpSocket::from_std(command_std_socket);

        let poll = Poll::new().map_err(|e| format!("Failed to create Poll: {}", e.to_string()))?;
        let events = Events::with_capacity(1);

        poll.registry()
            .register(&mut port, SERIAL_TOKEN, Interest::READABLE)
            .map_err(|e| {
                format!(
                    "Failed to register serial port with poll: {}",
                    e.to_string()
                )
            })?;
        poll.registry()
            .register(&mut command_socket, COMMAND_UDP_TOKEN, Interest::READABLE)
            .map_err(|e| {
                format!(
                    "Failed to register command UDP socket with poll: {}",
                    e.to_string()
                )
            })?;

        Ok(Self {
            msg_count: 0,
            dropped_count: 0,
            start_time: now,
            last_status_update: now,
            port,
            poll,
            events,
            decoder,
            status_only,
            telemetry_socket,
            command_socket,
        })
    }

    /// Processes a decoded log frame for telemetry purposes.
    /// Sends data via UDP for plotting.
    fn process_telemetry_log(&mut self, log: &cdefmt_decoder::log::Log) -> Result<(), String> {
        if let Some(args) = log.get_args() {
            let position = match args.get(0) {
                Some(&Var::U16(v)) => v,
                _ => return Err("arg 0 missing".into()),
            };

            let setpoint = match args.get(1) {
                Some(&Var::U16(v)) => v,
                _ => return Err("arg 1 missing".into()),
            };

            let pos_pid = match args.get(2) {
                Some(&Var::F32(v)) => v,
                _ => return Err("arg 2 missing".into()),
            };

            let speed_pid = match args.get(3) {
                Some(&Var::F32(v)) => v,
                _ => return Err("arg 3 missing".into()),
            };

            let speed = match args.get(4) {
                Some(&Var::F32(v)) => v,
                _ => return Err("arg 4 missing".into()),
            };

            let message = xyplot_data("Waveform1", "position", self.msg_count, position as f32);
            self.telemetry_socket
                .send(message.as_bytes())
                .map_err(|e| e.to_string())?;

            let message = xyplot_data("Waveform2", "setpoint", self.msg_count, setpoint as f32);
            self.telemetry_socket
                .send(message.as_bytes())
                .map_err(|e| e.to_string())?;

            let message = xyplot_data("Waveform3", "posPID", self.msg_count, pos_pid);
            self.telemetry_socket
                .send(message.as_bytes())
                .map_err(|e| e.to_string())?;

            let message = xyplot_data("Waveform4", "speedPID", self.msg_count, speed_pid);
            self.telemetry_socket
                .send(message.as_bytes())
                .map_err(|e| e.to_string())?;

            let message = xyplot_data("Waveform5", "speed", self.msg_count, speed);
            self.telemetry_socket
                .send(message.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Handles a single successfully COBS-decoded frame.
    fn handle_decoded_frame(&mut self, decoded: &[u8]) -> Result<(), String> {
        if decoded.len() < 4 {
            self.dropped_count += 1;
            eprintln!("Frame too short");
            return Ok(());
        }

        let log_len = u32::from_le_bytes(decoded[..4].try_into().unwrap());
        if decoded.len() - 4 != log_len as usize {
            self.dropped_count += 1;
            eprintln!("Length mismatch");
            return Ok(());
        }

        match self.decoder.decode_log(&decoded[4..]) {
            Ok(log) => {
                if let Err(e) = self.process_telemetry_log(&log) {
                    eprintln!("Failed to process telemetry log: {}", e);
                }
                self.msg_count += 1;
                if self.status_only {
                    let now = Instant::now();

                    if now > self.last_status_update {
                        let elapsed_secs = now.duration_since(self.start_time).as_secs_f64();
                        let rate = self.msg_count as f64 / elapsed_secs.max(1.0);

                        print!("\r{:<7} > {:<40} ", log.get_level(), log.to_string());

                        let drop_ratio =
                            (self.dropped_count as f64 / self.msg_count as f64).max(0.0) * 100.0;
                        print!(
                            "{:<7.2} msg/s (Total: {}, Dropped: {}, {:.2}%)   ",
                            rate, self.msg_count, self.dropped_count, drop_ratio
                        );
                        stdout().flush().map_err(|e| e.to_string())?;

                        self.last_status_update = now + Duration::from_millis(500);
                    }
                } else {
                    println!("{:<7} > {}", log.get_level(), log)
                }
            }
            Err(e) => {
                self.dropped_count += 1;
                eprintln!("Err: {}", e)
            }
        }

        Ok(())
    }

    fn process_events(&mut self) -> Result<(), String> {
        let mut serial_buf = [0u8; SERIAL_BUF_SIZE];
        let mut serial_start = 0;
        let mut serial_end = 0;

        let mut command_udp_buf = [0u8; MAX_UDP_PACKET_SIZE];

        loop {
            self.poll
                .poll(&mut self.events, None)
                .map_err(|e| e.to_string())?;

            let event = self.events.iter().next().unwrap(); // dont ever try a for loop here
            if event.token() == COMMAND_UDP_TOKEN {
                loop {
                    match self.command_socket.recv_from(&mut command_udp_buf) {
                        Ok((size, src_addr)) => {
                            let msg_str = String::from_utf8_lossy(&command_udp_buf[..size]);
                            if let Some(packet_to_send) = process_udp_message(&msg_str) {
                                if let Err(e) = self.port.write_all(&packet_to_send) {
                                    eprintln!("\nFailed to write command to serial port: {}", e);
                                }
                            } else {
                                eprintln!("\nInvalid command from {}: {}", src_addr, msg_str);
                            }
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => break, // No more UDP packets for now
                        Err(e) => eprintln!("\nCommand UDP receive error: {}", e), // Log error and continue
                    }
                }
            } else if event.token() == SERIAL_TOKEN {
                loop {
                    if serial_start > 0 && serial_start != serial_end {
                        serial_buf.copy_within(serial_start..serial_end, 0);
                        serial_end -= serial_start;
                        serial_start = 0;
                    } else if serial_start == serial_end {
                        serial_start = 0;
                        serial_end = 0;
                    }

                    let read_buf = &mut serial_buf[serial_end..];
                    let read = match self.port.read(read_buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => return Err(format!("Serial read error: {}", e)),
                    };
                    serial_end += read;

                    while let Some(pos) = serial_buf[serial_start..serial_end]
                        .iter()
                        .position(|&b| b == 0)
                    {
                        let frame_end = serial_start + pos;
                        let frame = &serial_buf[serial_start..frame_end];

                        let mut decoded = vec![0_u8; decode_max_output_size(frame.len())];
                        match decode_array(&mut decoded[..], frame) {
                            Ok(decoded_slice) => {
                                self.handle_decoded_frame(decoded_slice)?;
                            }
                            Err(_) => {
                                self.dropped_count += 1;
                                eprintln!("COBS decode error - skipping frame")
                            }
                        }
                        serial_start = frame_end + 1;
                    }

                    if serial_end == SERIAL_BUF_SIZE && serial_start == 0 {
                        return Err("Buffer full without finding frame boundary".to_string());
                    }
                }
            }
        }
    }
}

fn main() -> Result<(), String> {
    let args = Args::parse();

    let file = std::fs::File::open(&args.elf).map_err(|e| e.to_string())?;
    let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| e.to_string())?;
    let mut decoder = cdefmt_decoder::Decoder::new(&*mmap).map_err(|e| e.to_string())?;

    let count = decoder.precache_log_metadata().map_err(|e| e.to_string())?;
    println!("Precached {count} logs");

    let port = mio_serial::new(&args.port, args.baud_rate)
        .open_native_async()
        .map_err(|e| format!("Failed to open port '{}': {}", args.port, e))?;

    let mut log_processor = SerialGateway::new(
        port,
        decoder,
        args.status_only,
        args.telemetry_port,
        args.command_port,
    )?;

    println!(
        "Telemetry UDP socket configured to send to 127.0.0.1:{} for plot data.",
        args.telemetry_port
    );
    println!(
        "Command UDP server listening on port {} for serial commands.",
        args.command_port
    );

    log_processor.process_events()
}
