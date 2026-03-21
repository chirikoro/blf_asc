# blf_asc

A small Rust library for reading and writing Vector BLF and ASC (CAN log) files, modeled after python-can behavior.

[![crates.io](https://img.shields.io/crates/v/blf_asc.svg)](https://crates.io/crates/blf_asc) [![docs.rs](https://docs.rs/blf_asc/badge.svg)](https://docs.rs/blf_asc) [![license](https://img.shields.io/crates/l/blf_asc.svg)](https://github.com/chirikoro/blf_asc/blob/master/LICENSE-MIT)

Documentation: [docs.rs/blf_asc](https://docs.rs/blf_asc)

## Features

- BLF reader with `Iterator<Item = Message>`
- BLF writer with `on_message_received` (python-can-like)
- ASC (Vector) reader/writer
- Supports CAN classic, CAN FD, and error frames
- Zlib compression for BLF (default level = -1, same intent as python-can)

## Installation

```toml
[dependencies]
blf_asc = "0.1"
```

## Usage

### Read BLF

```rust
use blf_asc::{BlfReader, Result};

fn main() -> Result<()> {
    let mut reader = BlfReader::open("input.blf")?;
    for msg in reader.by_ref() {
        // msg: Message
        println!("id=0x{:X} dlc={} data={:02X?}", msg.arbitration_id, msg.dlc, msg.data);
    }
    if let Some(err) = reader.take_error() {
        eprintln!("reader error: {err}");
    }
    Ok(())
}
```

Note: `msg.data` is a `Vec<u8>`. To print hex, use `{:02X?}` or `msg.data_hex()`. The helper `msg.arbitration_id_hex()` prints the ID as hex.

### Write BLF (python-can style)

```rust
use blf_asc::{BlfWriter, Message, Result};

fn main() -> Result<()> {
    let mut writer = BlfWriter::create("output.blf")?; // default compression level = -1

    let msg = Message {
        timestamp: 0.0,
        arbitration_id: 0x123,
        is_extended_id: false,
        is_remote_frame: false,
        is_rx: true,
        is_error_frame: false,
        is_fd: false,
        bitrate_switch: false,
        error_state_indicator: false,
        dlc: 3,
        data: vec![0x11, 0x22, 0x33],
        channel: 0, // 0-based (channel 1 in BLF/ASC)
    };

    writer.on_message_received(&msg)?;
    writer.finish()?;
    Ok(())
}
```

### Read ASC

```rust
use blf_asc::{AscReader, Result};

fn main() -> Result<()> {
    let mut reader = AscReader::open("input.asc")?; // default: base hex, relative timestamps
    for msg in reader.by_ref() {
        println!("id=0x{:X} dlc={} data={:02X?}", msg.arbitration_id, msg.dlc, msg.data);
    }
    if let Some(err) = reader.take_error() {
        eprintln!("reader error: {err}");
    }
    Ok(())
}
```

### Write ASC

```rust
use blf_asc::{AscWriter, Message, Result};

fn main() -> Result<()> {
    let mut writer = AscWriter::create("output.asc")?;

    let msg = Message {
        timestamp: 1710000000.123,
        arbitration_id: 0x123,
        is_extended_id: false,
        is_remote_frame: false,
        is_rx: true,
        is_error_frame: false,
        is_fd: false,
        bitrate_switch: false,
        error_state_indicator: false,
        dlc: 3,
        data: vec![0x11, 0x22, 0x33],
        channel: 0,
    };

    writer.on_message_received(&msg)?;
    writer.finish()?;
    Ok(())
}
```

### Convert BLF <-> ASC

```rust
use blf_asc::{AscReader, AscWriter, BlfReader, BlfWriter, Result};

fn blf_to_asc(input: &str, output: &str) -> Result<()> {
    let reader = BlfReader::open(input)?;
    let mut writer = AscWriter::create(output)?;
    for msg in reader {
        writer.on_message_received(&msg)?;
    }
    writer.finish()?;
    Ok(())
}

fn asc_to_blf(input: &str, output: &str) -> Result<()> {
    // Use absolute timestamps from ASC header/triggerblock.
    let reader = AscReader::open_with_options(input, "hex", false)?;
    let mut writer = BlfWriter::create(output)?;
    for msg in reader {
        writer.on_message_received(&msg)?;
    }
    writer.finish()?;
    Ok(())
}
```

## Notes

- `Message::channel` is 0-based. It is serialized as channel+1 in BLF/ASC, matching python-can conventions.
- BLF object types supported: CAN_MESSAGE, CAN_MESSAGE2, CAN_ERROR_EXT, CAN_FD_MESSAGE, CAN_FD_MESSAGE_64.
- ASC timestamp precision is milliseconds. A BLF -> ASC -> BLF roundtrip will lose sub-millisecond precision.

## License

Licensed under either of

- Apache License, Version 2.0
- MIT license

at your option.

## Release

This repo includes `release.toml` for [cargo-release](https://crates.io/crates/cargo-release).

Typical flow:
1. cargo release patch
