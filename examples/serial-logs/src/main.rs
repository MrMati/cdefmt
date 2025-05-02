use std::{
    io::{stdout, Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use clap::Parser;

#[derive(clap::Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the ELF file containing log format strings.
    #[arg(short, long)]
    elf: PathBuf,

    /// Serial port name (e.g., COM3 on Windows, /dev/ttyACM0 on Linux).
    #[arg(short, long)]
    port: String,

    /// Serial port baud rate.
    #[arg(short, long, default_value_t = 115200)]
    baud_rate: u32,

    /// Only display message rate status, hide individual logs.
    #[arg(long, default_value_t = false)]
    status_only: bool,
}

/// Processes a single message read from the serial port.
/// Returns Ok(true) to continue processing, Ok(false) to break the loop gracefully.
/// Returns Err(String) on fatal errors.
fn process_message(
    port: &mut Box<dyn serialport::SerialPort>,
    decoder: &mut cdefmt_decoder::Decoder,
    len_buf: &[u8; std::mem::size_of::<u32>()],
    msg_count: &mut u64,
    start_time: Instant,
    last_status_update: &mut Instant,
    status_only: bool,
) -> Result<bool, String> {
    let len = u32::from_le_bytes(*len_buf);

    // Basic sanity check for length to prevent huge allocations
    if len > 10 * 1024 {
        // Limit to 10 KiB, adjust as needed
        eprintln!(
            "\nErr: Received excessive length ({}), skipping message.",
            len
        );
        // Consider adding logic here to try and re-synchronize the stream if necessary
        return Ok(true); // Continue processing next message
    }
    if len == 0 {
        // Skip zero-length messages if they occur
        return Ok(true); // Continue processing next message
    }

    let mut buff = vec![0; len as usize];

    if let Err(e) = port.read_exact(buff.as_mut_slice()) {
        // Print newline if needed to avoid overwriting status line
        if status_only {
            println!();
        }
        eprintln!("Err: Failed to read log data (length {}): {}", len, e);
        return Ok(false); // Indicate loop should break
    }

    *msg_count += 1;

    if status_only {
        let now = Instant::now();
        if now.duration_since(*last_status_update) >= Duration::from_secs(1) {
            let elapsed_secs = now.duration_since(start_time).as_secs_f64();
            let rate = if elapsed_secs > 0.0 {
                *msg_count as f64 / elapsed_secs
            } else {
                0.0
            };
            print!("\r{:<7.2} msg/s (Total: {})   ", rate, *msg_count); // Pad with spaces
            stdout().flush().map_err(|e| e.to_string())?;
            *last_status_update = now;
        }
    } else {
        match decoder.decode_log(&buff) {
            Ok(log) => println!("{:<7} > {}", log.get_level(), log),
            Err(e) => eprintln!("Err decoding log: {}", e), // Use eprintln for errors
        }
    }
    Ok(true) // Indicate loop should continue
}

/// Takes path to elf as argument, and parses logs whose IDs are read from serial port.
fn main() -> std::result::Result<(), String> {
    let args = Args::parse();

    let file = std::fs::File::open(args.elf).map_err(|e| e.to_string())?;

    let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| e.to_string())?;

    let mut decoder = cdefmt_decoder::Decoder::new(&*mmap).map_err(|e| e.to_string())?;

    let count = decoder.precache_log_metadata().map_err(|e| e.to_string())?;

    println!("Precached {count} logs");

    println!(
        "Attempting to open serial port {} at {} baud",
        args.port, args.baud_rate
    );

    // Open the serial port
    let mut port = serialport::new(&args.port, args.baud_rate)
        .timeout(Duration::from_millis(1000)) // Timeout breaks sparse logs
        .open()
        .map_err(|e| format!("Failed to open port '{}': {}", args.port, e))?;

    let mut len_buf = [0; std::mem::size_of::<u32>()];
    let mut msg_count: u64 = 0;
    let start_time = Instant::now();
    let mut last_status_update = Instant::now();

    loop {
        match port.read_exact(&mut len_buf) {
            Ok(()) => {
                // Successfully read length, process the message
                match process_message(
                    &mut port,
                    &mut decoder,
                    &len_buf,
                    &mut msg_count,
                    start_time,
                    &mut last_status_update,
                    args.status_only,
                ) {
                    Ok(true) => continue,    // Message processed, continue loop
                    Ok(false) => break,      // Graceful break requested (e.g., data read error)
                    Err(e) => return Err(e), // Fatal error during processing
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                // Timeout occurred while waiting for length, just retry
                continue;
            }
            Err(e) => {
                // Other I/O error occurred while reading length, break the loop
                if args.status_only { println!(); } // Avoid overwriting status line
                eprintln!("Err: Failed to read message length: {}", e);
                break;
            }
        }
    }

    // Clean up after the loop finishes
    if args.status_only {
        println!(); // Print a newline to move off the status line
    }

    println!("Serial port closed or read error occurred.");

    Ok(())
}
