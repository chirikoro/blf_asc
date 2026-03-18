# codex_blf

Vector BLF と ASC (CAN ログ) を読み書きする Rust ライブラリです。python-can の挙動を参考にしています。

## 特長

- BLF リーダは `Iterator<Item = Message>` を返却
- BLF ライタは `on_message_received` で逐次書き込み (python-can 風)
- ASC (Vector) の読み書きに対応
- CAN クラシック / CAN FD / エラーフレームをサポート
- BLF は zlib 圧縮 (デフォルト圧縮レベル = -1)

## インストール

```toml
[dependencies]
codex_blf = "0.1"
```

## 使い方

### BLF を読む

```rust
use codex_blf::{BlfReader, Result};

fn main() -> Result<()> {
    let mut reader = BlfReader::open("input.blf")?;
    for msg in reader.by_ref() {
        println!("id=0x{:X} dlc={} data={:02X?}", msg.arbitration_id, msg.dlc, msg.data);
    }
    if let Some(err) = reader.take_error() {
        eprintln!("reader error: {err}");
    }
    Ok(())
}
```

### BLF を書く (python-can 風)

```rust
use codex_blf::{BlfWriter, Message, Result};

fn main() -> Result<()> {
    let mut writer = BlfWriter::create("output.blf")?; // デフォルト圧縮レベル = -1

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
        channel: 0, // 0-based (BLF/ASC では 1 チャンネル)
    };

    writer.on_message_received(&msg)?;
    writer.finish()?;
    Ok(())
}
```

### ASC を読む

```rust
use codex_blf::{AscReader, Result};

fn main() -> Result<()> {
    let mut reader = AscReader::open("input.asc")?; // 既定: base hex, 相対時刻
    for msg in reader.by_ref() {
        println!("id=0x{:X} dlc={} data={:02X?}", msg.arbitration_id, msg.dlc, msg.data);
    }
    if let Some(err) = reader.take_error() {
        eprintln!("reader error: {err}");
    }
    Ok(())
}
```

### ASC を書く

```rust
use codex_blf::{AscWriter, Message, Result};

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

### BLF <-> ASC 変換

```rust
use codex_blf::{AscReader, AscWriter, BlfReader, BlfWriter, Result};

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
    // ASC ヘッダ / Triggerblock から絶対時刻を使う
    let reader = AscReader::open_with_options(input, "hex", false)?;
    let mut writer = BlfWriter::create(output)?;
    for msg in reader {
        writer.on_message_received(&msg)?;
    }
    writer.finish()?;
    Ok(())
}
```

## 注意点

- `Message::channel` は 0-based です (BLF/ASC では +1 されて保存されます)。
- 対応する BLF オブジェクト: CAN_MESSAGE / CAN_MESSAGE2 / CAN_ERROR_EXT / CAN_FD_MESSAGE / CAN_FD_MESSAGE_64。
- ASC の時刻精度はミリ秒です。BLF -> ASC -> BLF の往復ではサブミリ秒が失われます。

## ライセンス

以下のいずれかを選択できます。

- Apache License, Version 2.0
- MIT license

