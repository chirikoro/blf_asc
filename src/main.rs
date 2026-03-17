use codex_blf::{BlfError, BlfReader, BlfWriter, Message, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
struct JsonMessage {
    timestamp: f64,
    arbitration_id: u32,
    is_extended_id: bool,
    is_remote_frame: bool,
    is_rx: bool,
    is_error_frame: bool,
    is_fd: bool,
    bitrate_switch: bool,
    error_state_indicator: bool,
    dlc: u8,
    data: Vec<u8>,
    channel: u16,
}

impl From<Message> for JsonMessage {
    fn from(msg: Message) -> Self {
        Self {
            timestamp: msg.timestamp,
            arbitration_id: msg.arbitration_id,
            is_extended_id: msg.is_extended_id,
            is_remote_frame: msg.is_remote_frame,
            is_rx: msg.is_rx,
            is_error_frame: msg.is_error_frame,
            is_fd: msg.is_fd,
            bitrate_switch: msg.bitrate_switch,
            error_state_indicator: msg.error_state_indicator,
            dlc: msg.dlc,
            data: msg.data,
            channel: msg.channel,
        }
    }
}

impl From<JsonMessage> for Message {
    fn from(msg: JsonMessage) -> Self {
        Self {
            timestamp: msg.timestamp,
            arbitration_id: msg.arbitration_id,
            is_extended_id: msg.is_extended_id,
            is_remote_frame: msg.is_remote_frame,
            is_rx: msg.is_rx,
            is_error_frame: msg.is_error_frame,
            is_fd: msg.is_fd,
            bitrate_switch: msg.bitrate_switch,
            error_state_indicator: msg.error_state_indicator,
            dlc: msg.dlc,
            data: msg.data,
            channel: msg.channel,
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage:");
        println!("  {} <input.blf> <output.blf>", args[0]);
        println!("  {} --dump <input.blf> <count>", args[0]);
        println!("  {} read <input.blf> [--limit N]", args[0]);
        println!("  {} write <output.blf> <input.jsonl>", args[0]);
        return Ok(());
    }

    if args[1] == "read" {
        if args.len() < 3 {
            println!("Usage: {} read <input.blf> [--limit N]", args[0]);
            return Ok(());
        }
        let input = Path::new(&args[2]);
        let mut limit: Option<usize> = None;
        if args.len() == 5 && args[3] == "--limit" {
            limit = args[4].parse().ok();
        }
        let mut reader = BlfReader::open(input)?;
        let mut count = 0usize;
        for msg in reader.by_ref() {
            let json = JsonMessage::from(msg);
            println!(
                "{}",
                serde_json::to_string(&json).map_err(|e| BlfError::Parse(e.to_string()))?
            );
            count += 1;
            if let Some(max) = limit {
                if count >= max {
                    break;
                }
            }
        }
        if let Some(err) = reader.take_error() {
            return Err(err);
        }
        return Ok(());
    }

    if args[1] == "write" {
        if args.len() != 4 {
            println!("Usage: {} write <output.blf> <input.jsonl>", args[0]);
            return Ok(());
        }
        let output = Path::new(&args[2]);
        let input_path = Path::new(&args[3]);
        let input_file = File::open(input_path)?;
        let reader = BufReader::new(input_file);
        let mut writer = BlfWriter::create(output)?;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let json: JsonMessage =
                serde_json::from_str(&line).map_err(|e| BlfError::Parse(e.to_string()))?;
            let msg: Message = json.into();
            writer.on_message_received(&msg)?;
        }
        writer.finish()?;
        return Ok(());
    }

    if args[1] == "--dump" {
        if args.len() != 4 {
            println!("Usage: {} --dump <input.blf> <count>", args[0]);
            return Ok(());
        }
        let input = Path::new(&args[2]);
        let count: usize = args[3].parse().unwrap_or(10);
        let mut reader = BlfReader::open(input)?;
        for idx in 0..count {
            match reader.next_message()? {
                Some(msg) => println!("{idx}: {msg:?}"),
                None => break,
            }
        }
        return Ok(());
    }

    if args.len() != 3 {
        println!("Usage: {} <input.blf> <output.blf>", args[0]);
        return Ok(());
    }
    let input = Path::new(&args[1]);
    let output = Path::new(&args[2]);

    let mut reader = BlfReader::open(input)?;
    let mut writer = BlfWriter::create(output)?;

    let mut count: u64 = 0;
    for msg in reader.by_ref() {
        writer.on_message_received(&msg)?;
        count += 1;
        if count % 200_000 == 0 {
            println!("processed {count} messages...");
        }
    }
    if let Some(err) = reader.take_error() {
        return Err(err);
    }
    writer.finish()?;
    println!("done: wrote {count} messages");
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
