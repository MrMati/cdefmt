use clap::Parser;
use cobs2::cobsr::{decode_array, decode_max_output_size};
use serialport::SerialPort;
use std::{
    io::{stdout, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use std::net::UdpSocket;
use cdefmt_decoder::var::Var;

/// CLI args
#[derive(clap::Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the ELF file containing log format strings.
    #[arg(short, long)]
    elf: PathBuf,

    /// Serial port name (e.g., COM3 on Windows, /dev/ttyACM0).
    #[arg(short, long)]
    port: String,

    /// Serial port baud rate.
    #[arg(short, long, default_value_t = 115200)]
    baud_rate: u32,

    /// Only display message rate status, hide individual logs.
    #[arg(long, default_value_t = false)]
    status_only: bool,
}

fn timeplot_data(channel: &str, series: &str, value: u32) -> String {
    format!("{{TIMEPLOT:{}|DATA|{}|T|{:.2}}}\n", channel, series, value)
}

fn xyplot_data(channel: &str, series: &str, x: u64, y: u32) -> String {
    format!("{{XYPLOT:{}|DATA|{}|{}|{}}}", 
            channel, series, x, y)
}

const BUF_SIZE: usize = 64;

struct LogProcessor<'a> {
    msg_count: u64,
    dropped_count: u64,
    start_time: Instant,
    last_status_update: Instant,
    port: Box<dyn SerialPort>,
    // Takes ownership of the Decoder value, but Decoder still borrows mmap data
    decoder: cdefmt_decoder::Decoder<'a>,
    status_only: bool,
    socket: UdpSocket
}

impl<'a> LogProcessor<'a> {
    // Updated constructor to take owned values
    fn new(
        port: Box<dyn SerialPort>,
        decoder: cdefmt_decoder::Decoder<'a>,
        status_only: bool,
        socket: UdpSocket
    ) -> Self {
        let now = Instant::now();



        Self {
            msg_count: 0,
            dropped_count: 0,
            start_time: now,
            last_status_update: now,
            port,
            decoder,
            status_only,
            socket
        }
    }

    /// Handles a single successfully COBS-decoded frame.
    fn handle_decoded_frame(&mut self, decoded: &[u8]) -> Result<(), String> {
        if decoded.len() < 4 {
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

                let value = log.get_args().unwrap().first().unwrap();
                match value {
                    Var::U32(v) => {
                        //let message = timeplot_data("Waveform", "ADC", *v);
                        let message = xyplot_data("Waveform", "ADC", self.msg_count, *v);
                        self.socket.send(message.as_bytes()).unwrap();
                    }
                    _ => {}
                }
                
                self.msg_count += 1;
                if self.status_only {
                    
                    let now = Instant::now();

                    if now.duration_since(self.last_status_update) >= Duration::from_secs(1) {
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

                        self.last_status_update = now;
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

    /// Processes the input stream, reading COBS-encoded frames
    fn process_serial_stream(&mut self) -> Result<(), String> {
        let mut buf = [0u8; BUF_SIZE];
        let mut start = 0;
        let mut end = 0;

        loop {
            if start > 0 && start != end {
                buf.copy_within(start..end, 0);
                end -= start;
                start = 0;
            } else if start == end {
                start = 0;
                end = 0;
            }

            let read_buf = &mut buf[end..];
            let read = match self.port.read(read_buf) {
                Ok(n) => n,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
                Err(e) => return Err(format!("Serial read error: {}", e)),
            };
            end += read;

            while let Some(pos) = buf[start..end].iter().position(|&b| b == 0) {
                let frame_end = start + pos;
                let frame = &mut buf[start..frame_end];

                let mut decoded = vec![0_u8; decode_max_output_size(frame.len())];
                match decode_array(&mut decoded[..], frame) {
                    Ok(out_len) => {
                        //let decoded = &frame[..out_len];
                        self.handle_decoded_frame(out_len)?;
                    }
                    Err(_) => {
                        self.dropped_count += 1;
                        eprintln!("COBS decode error - skipping frame")
                    }
                }
                start = frame_end + 1;
            }

            if end == BUF_SIZE && start == 0 {
                return Err("Buffer full without finding frame boundary".to_string());
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

    let port = serialport::new(&args.port, args.baud_rate)
        .timeout(Duration::from_millis(100))
        .open()
        .map_err(|e| format!("Failed to open port '{}': {}", args.port, e))?;

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Failed to bind UDP 0.0.0.0 : {}", e))?;
    socket.connect("127.0.0.1:8888").map_err(|e| format!("Failed to connect UDP :8888 : {}", e))?;

    // Pass owned port and decoder to the constructor
    let mut log_processor = LogProcessor::new(port, decoder, args.status_only, socket);
    log_processor.process_serial_stream()
}
