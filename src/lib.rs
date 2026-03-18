use chrono::{Datelike, Local, TimeZone, Timelike, Utc};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

const FILE_HEADER_STRUCT_SIZE: usize = 72;
const FILE_HEADER_SIZE: u32 = 144;

const OBJ_HEADER_BASE_SIZE: usize = 16;
const OBJ_HEADER_V1_SIZE: usize = 16;
const OBJ_HEADER_V2_SIZE: usize = 24;

const LOG_CONTAINER_SIZE: usize = 16;
const CAN_MSG_SIZE: usize = 16;
const CAN_FD_MSG_SIZE: usize = 84;
const CAN_FD_MSG_64_SIZE: usize = 40;
const CAN_ERROR_EXT_SIZE: usize = 32;

const CAN_MESSAGE: u32 = 1;
const LOG_CONTAINER: u32 = 10;
const CAN_ERROR_EXT: u32 = 73;
const CAN_MESSAGE2: u32 = 86;
const CAN_FD_MESSAGE: u32 = 100;
const CAN_FD_MESSAGE_64: u32 = 101;

const NO_COMPRESSION: u16 = 0;
const ZLIB_DEFLATE: u16 = 2;

const CAN_MSG_EXT: u32 = 0x8000_0000;
const REMOTE_FLAG: u8 = 0x80;
const EDL: u8 = 0x1;
const BRS: u8 = 0x2;
const ESI: u8 = 0x4;
const DIR: u8 = 0x1;

const TIME_TEN_MICS: u32 = 0x0000_0001;
const TIME_ONE_NANS: u32 = 0x0000_0002;

const CAN_FD_DLC: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

#[derive(Debug)]
pub enum BlfError {
    Io(io::Error),
    Parse(String),
}

impl fmt::Display for BlfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlfError::Io(err) => write!(f, "io error: {err}"),
            BlfError::Parse(msg) => write!(f, "parse error: {msg}"),
        }
    }
}

impl std::error::Error for BlfError {}

impl From<io::Error> for BlfError {
    fn from(err: io::Error) -> Self {
        BlfError::Io(err)
    }
}

pub type Result<T> = std::result::Result<T, BlfError>;

#[derive(Clone, Debug)]
pub struct Message {
    pub timestamp: f64,
    pub arbitration_id: u32,
    pub is_extended_id: bool,
    pub is_remote_frame: bool,
    pub is_rx: bool,
    pub is_error_frame: bool,
    pub is_fd: bool,
    pub bitrate_switch: bool,
    pub error_state_indicator: bool,
    pub dlc: u8,
    pub data: Vec<u8>,
    pub channel: u16,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            timestamp: 0.0,
            arbitration_id: 0,
            is_extended_id: false,
            is_remote_frame: false,
            is_rx: true,
            is_error_frame: false,
            is_fd: false,
            bitrate_switch: false,
            error_state_indicator: false,
            dlc: 0,
            data: Vec::new(),
            channel: 0,
        }
    }
}

fn dlc2len(dlc: u8) -> u8 {
    if dlc <= 15 {
        CAN_FD_DLC[dlc as usize]
    } else {
        64
    }
}

fn len2dlc(length: u8) -> u8 {
    if length <= 8 {
        return length;
    }
    for (dlc, &bytes) in CAN_FD_DLC.iter().enumerate() {
        if bytes >= length {
            return dlc as u8;
        }
    }
    15
}

fn timestamp_to_systemtime(timestamp: Option<f64>) -> [u16; 8] {
    match timestamp {
        Some(ts) if ts >= 631_152_000.0 => {
            let rounded_ms = (ts * 1000.0).round();
            let secs = (rounded_ms / 1000.0).floor() as i64;
            let millis = (rounded_ms - (secs as f64 * 1000.0)).round() as u32;
            if let Some(dt) = Utc.timestamp_opt(secs, millis * 1_000_000).single() {
                let weekday = dt.weekday().number_from_monday() % 7;
                [
                    dt.year() as u16,
                    dt.month() as u16,
                    weekday as u16,
                    dt.day() as u16,
                    dt.hour() as u16,
                    dt.minute() as u16,
                    dt.second() as u16,
                    dt.timestamp_subsec_millis() as u16,
                ]
            } else {
                [0; 8]
            }
        }
        _ => [0; 8],
    }
}

fn systemtime_to_timestamp(systemtime: [u16; 8]) -> f64 {
    let year = systemtime[0] as i32;
    let month = systemtime[1] as u32;
    let day = systemtime[3] as u32;
    let hour = systemtime[4] as u32;
    let minute = systemtime[5] as u32;
    let second = systemtime[6] as u32;
    let millis = systemtime[7] as u32;

    if year == 0 || month == 0 || day == 0 {
        return 0.0;
    }

    if let Some(dt) = Utc
        .with_ymd_and_hms(year, month, day, hour, minute, second)
        .single()
    {
        let dt = dt + chrono::Duration::milliseconds(millis as i64);
        dt.timestamp_millis() as f64 / 1000.0
    } else {
        0.0
    }
}

fn read_exact_or_eof(file: &mut File, len: usize) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; len];
    let mut read_total = 0usize;
    while read_total < len {
        let read_now = file.read(&mut buf[read_total..])?;
        if read_now == 0 {
            if read_total == 0 {
                return Ok(None);
            }
            return Err(BlfError::Parse("unexpected EOF".into()));
        }
        read_total += read_now;
    }
    Ok(Some(buf))
}

fn read_u16_le(data: &[u8], offset: usize) -> Result<u16> {
    if offset + 2 > data.len() {
        return Err(BlfError::Parse("buffer too small for u16".into()));
    }
    Ok(u16::from_le_bytes([data[offset], data[offset + 1]]))
}

fn read_u32_le(data: &[u8], offset: usize) -> Result<u32> {
    if offset + 4 > data.len() {
        return Err(BlfError::Parse("buffer too small for u32".into()));
    }
    Ok(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

fn read_u64_le(data: &[u8], offset: usize) -> Result<u64> {
    if offset + 8 > data.len() {
        return Err(BlfError::Parse("buffer too small for u64".into()));
    }
    Ok(u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]))
}

pub struct BlfReader {
    file: File,
    start_timestamp: f64,
    tail: Vec<u8>,
    pending: VecDeque<Message>,
    error: Option<BlfError>,
}

impl BlfReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut file = File::open(path)?;
        let header = read_exact_or_eof(&mut file, FILE_HEADER_STRUCT_SIZE)?
            .ok_or_else(|| BlfError::Parse("empty file".into()))?;

        if &header[0..4] != b"LOGG" {
            return Err(BlfError::Parse("unexpected file signature".into()));
        }
        let header_size = read_u32_le(&header, 4)? as usize;
        if header_size < FILE_HEADER_STRUCT_SIZE {
            return Err(BlfError::Parse("invalid header size".into()));
        }

        let start_systemtime = {
            let mut arr = [0u16; 8];
            for i in 0..8 {
                arr[i] = read_u16_le(&header, 40 + i * 2)?;
            }
            arr
        };

        let start_timestamp = systemtime_to_timestamp(start_systemtime);

        if header_size > FILE_HEADER_STRUCT_SIZE {
            let mut skip = vec![0u8; header_size - FILE_HEADER_STRUCT_SIZE];
            file.read_exact(&mut skip)?;
        }

        Ok(Self {
            file,
            start_timestamp,
            tail: Vec::new(),
            pending: VecDeque::new(),
            error: None,
        })
    }

    pub fn next_message(&mut self) -> Result<Option<Message>> {
        if let Some(msg) = self.pending.pop_front() {
            return Ok(Some(msg));
        }

        loop {
            let container = match self.read_next_container()? {
                Some(data) => data,
                None => return Ok(None),
            };
            let messages = self.parse_container(&container)?;
            if !messages.is_empty() {
                self.pending = VecDeque::from(messages);
                return Ok(self.pending.pop_front());
            }
        }
    }

    pub fn take_error(&mut self) -> Option<BlfError> {
        self.error.take()
    }

    fn read_next_container(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let base = match read_exact_or_eof(&mut self.file, OBJ_HEADER_BASE_SIZE)? {
                Some(data) => data,
                None => return Ok(None),
            };
            if &base[0..4] != b"LOBJ" {
                return Err(BlfError::Parse("unexpected object signature".into()));
            }
            let obj_size = read_u32_le(&base, 8)? as usize;
            let obj_type = read_u32_le(&base, 12)?;
            if obj_size < OBJ_HEADER_BASE_SIZE {
                return Err(BlfError::Parse("invalid object size".into()));
            }
            let data_len = obj_size - OBJ_HEADER_BASE_SIZE;
            let mut obj_data = vec![0u8; data_len];
            self.file.read_exact(&mut obj_data)?;
            let padding = obj_size % 4;
            if padding != 0 {
                let mut pad = vec![0u8; padding];
                self.file.read_exact(&mut pad)?;
            }

            if obj_type != LOG_CONTAINER {
                continue;
            }
            if data_len < LOG_CONTAINER_SIZE {
                return Err(BlfError::Parse("container header too small".into()));
            }
            let method = read_u16_le(&obj_data, 0)?;
            let container_data = &obj_data[LOG_CONTAINER_SIZE..];
            match method {
                NO_COMPRESSION => return Ok(Some(container_data.to_vec())),
                ZLIB_DEFLATE => {
                    let mut decoder = ZlibDecoder::new(container_data);
                    let mut out = Vec::new();
                    decoder.read_to_end(&mut out)?;
                    return Ok(Some(out));
                }
                _ => continue,
            }
        }
    }

    fn parse_container(&mut self, data: &[u8]) -> Result<Vec<Message>> {
        let mut buffer = Vec::with_capacity(self.tail.len() + data.len());
        if !self.tail.is_empty() {
            buffer.extend_from_slice(&self.tail);
            self.tail.clear();
        }
        buffer.extend_from_slice(data);

        let mut messages = Vec::new();
        let mut pos = 0usize;
        let max_pos = buffer.len();

        loop {
            let tail_start = pos;
            let search_end = std::cmp::min(pos + 8, max_pos);
            let mut found = None;
            let mut i = pos;
            while i + 4 <= search_end {
                if &buffer[i..i + 4] == b"LOBJ" {
                    found = Some(i);
                    break;
                }
                i += 1;
            }
            let obj_start = match found {
                Some(v) => v,
                None => {
                    if pos + 8 > max_pos {
                        self.tail = buffer[tail_start..].to_vec();
                        break;
                    }
                    return Err(BlfError::Parse("could not find next object".into()));
                }
            };

            if obj_start + OBJ_HEADER_BASE_SIZE > max_pos {
                self.tail = buffer[tail_start..].to_vec();
                break;
            }

            let header_size = read_u16_le(&buffer, obj_start + 4)?;
            let header_version = read_u16_le(&buffer, obj_start + 6)?;
            let obj_size = read_u32_le(&buffer, obj_start + 8)? as usize;
            let obj_type = read_u32_le(&buffer, obj_start + 12)?;

            if obj_start + obj_size > max_pos {
                self.tail = buffer[tail_start..].to_vec();
                break;
            }

            let mut cursor = obj_start + OBJ_HEADER_BASE_SIZE;
            let (flags, rel_timestamp) = match header_version {
                1 => {
                    if cursor + OBJ_HEADER_V1_SIZE > max_pos {
                        self.tail = buffer[tail_start..].to_vec();
                        break;
                    }
                    let flags = read_u32_le(&buffer, cursor)?;
                    let timestamp = read_u64_le(&buffer, cursor + 8)?;
                    cursor += OBJ_HEADER_V1_SIZE;
                    (flags, timestamp)
                }
                2 => {
                    if cursor + OBJ_HEADER_V2_SIZE > max_pos {
                        self.tail = buffer[tail_start..].to_vec();
                        break;
                    }
                    let flags = read_u32_le(&buffer, cursor)?;
                    let timestamp = read_u64_le(&buffer, cursor + 8)?;
                    cursor += OBJ_HEADER_V2_SIZE;
                    (flags, timestamp)
                }
                _ => {
                    pos = obj_start + obj_size;
                    continue;
                }
            };

            let factor = if flags == TIME_TEN_MICS { 1e-5 } else { 1e-9 };
            let timestamp = self.start_timestamp + (rel_timestamp as f64) * factor;

            match obj_type {
                CAN_MESSAGE | CAN_MESSAGE2 => {
                    if cursor + CAN_MSG_SIZE > obj_start + obj_size {
                        return Err(BlfError::Parse("CAN message too small".into()));
                    }
                    let channel = read_u16_le(&buffer, cursor)?;
                    let flags = buffer[cursor + 2];
                    let dlc = buffer[cursor + 3];
                    let can_id = read_u32_le(&buffer, cursor + 4)?;
                    let data_start = cursor + 8;
                    let data_len = std::cmp::min(dlc as usize, 8);
                    let mut data = Vec::with_capacity(data_len);
                    data.extend_from_slice(&buffer[data_start..data_start + data_len]);
                    messages.push(Message {
                        timestamp,
                        arbitration_id: can_id & 0x1FFF_FFFF,
                        is_extended_id: (can_id & CAN_MSG_EXT) != 0,
                        is_remote_frame: (flags & REMOTE_FLAG) != 0,
                        is_rx: (flags & DIR) == 0,
                        is_error_frame: false,
                        is_fd: false,
                        bitrate_switch: false,
                        error_state_indicator: false,
                        dlc,
                        data,
                        channel: channel.saturating_sub(1),
                    });
                }
                CAN_ERROR_EXT => {
                    if cursor + CAN_ERROR_EXT_SIZE > obj_start + obj_size {
                        return Err(BlfError::Parse("CAN error frame too small".into()));
                    }
                    let channel = read_u16_le(&buffer, cursor)?;
                    let dlc = buffer[cursor + 10];
                    let can_id = read_u32_le(&buffer, cursor + 16)?;
                    let data_start = cursor + 24;
                    let data_len = std::cmp::min(dlc as usize, 8);
                    let mut data = Vec::with_capacity(data_len);
                    data.extend_from_slice(&buffer[data_start..data_start + data_len]);
                    messages.push(Message {
                        timestamp,
                        arbitration_id: can_id & 0x1FFF_FFFF,
                        is_extended_id: (can_id & CAN_MSG_EXT) != 0,
                        is_remote_frame: false,
                        is_rx: true,
                        is_error_frame: true,
                        is_fd: false,
                        bitrate_switch: false,
                        error_state_indicator: false,
                        dlc,
                        data,
                        channel: channel.saturating_sub(1),
                    });
                }
                CAN_FD_MESSAGE => {
                    if cursor + CAN_FD_MSG_SIZE > obj_start + obj_size {
                        return Err(BlfError::Parse("CAN FD message too small".into()));
                    }
                    let channel = read_u16_le(&buffer, cursor)?;
                    let flags = buffer[cursor + 2];
                    let dlc_code = buffer[cursor + 3];
                    let can_id = read_u32_le(&buffer, cursor + 4)?;
                    let fd_flags = buffer[cursor + 13];
                    let valid_bytes = buffer[cursor + 14] as usize;
                    let data_start = cursor + 20;
                    let data_len = std::cmp::min(valid_bytes, 64);
                    let mut data = Vec::with_capacity(data_len);
                    data.extend_from_slice(&buffer[data_start..data_start + data_len]);
                    messages.push(Message {
                        timestamp,
                        arbitration_id: can_id & 0x1FFF_FFFF,
                        is_extended_id: (can_id & CAN_MSG_EXT) != 0,
                        is_remote_frame: (flags & REMOTE_FLAG) != 0,
                        is_rx: (flags & DIR) == 0,
                        is_error_frame: false,
                        is_fd: (fd_flags & EDL) != 0,
                        bitrate_switch: (fd_flags & BRS) != 0,
                        error_state_indicator: (fd_flags & ESI) != 0,
                        dlc: dlc2len(dlc_code),
                        data,
                        channel: channel.saturating_sub(1),
                    });
                }
                CAN_FD_MESSAGE_64 => {
                    if cursor + CAN_FD_MSG_64_SIZE > obj_start + obj_size {
                        return Err(BlfError::Parse("CAN FD 64 message too small".into()));
                    }
                    let channel = buffer[cursor];
                    let dlc_code = buffer[cursor + 1];
                    let valid_bytes = buffer[cursor + 2] as usize;
                    let can_id = read_u32_le(&buffer, cursor + 4)?;
                    let fd_flags = read_u32_le(&buffer, cursor + 12)?;
                    let direction = buffer[cursor + 34];
                    let ext_data_offset = buffer[cursor + 35] as usize;

                    let header_size_usize = header_size as usize;
                    let data_field_end = if ext_data_offset != 0 {
                        ext_data_offset
                    } else {
                        obj_size
                    };
                    let mut data_field_length = data_field_end
                        .saturating_sub(header_size_usize + CAN_FD_MSG_64_SIZE);
                    if data_field_length > valid_bytes {
                        data_field_length = valid_bytes;
                    }
                    let msg_data_offset = cursor + CAN_FD_MSG_64_SIZE;
                    let mut data = Vec::new();
                    if msg_data_offset + data_field_length <= buffer.len() {
                        data.extend_from_slice(
                            &buffer[msg_data_offset..msg_data_offset + data_field_length],
                        );
                    }
                    if data.len() < valid_bytes {
                        data.resize(valid_bytes, 0);
                    }

                    messages.push(Message {
                        timestamp,
                        arbitration_id: can_id & 0x1FFF_FFFF,
                        is_extended_id: (can_id & CAN_MSG_EXT) != 0,
                        is_remote_frame: (fd_flags & 0x0010) != 0,
                        is_rx: direction == 0,
                        is_error_frame: false,
                        is_fd: (fd_flags & 0x1000) != 0,
                        bitrate_switch: (fd_flags & 0x2000) != 0,
                        error_state_indicator: (fd_flags & 0x4000) != 0,
                        dlc: dlc2len(dlc_code),
                        data,
                        channel: channel.saturating_sub(1) as u16,
                    });
                }
                _ => {
                    // Ignore unsupported object types
                }
            }

            pos = obj_start + obj_size;
        }

        Ok(messages)
    }
}

impl Iterator for BlfReader {
    type Item = Message;

    fn next(&mut self) -> Option<Self::Item> {
        if self.error.is_some() {
            return None;
        }
        match self.next_message() {
            Ok(Some(msg)) => Some(msg),
            Ok(None) => None,
            Err(err) => {
                self.error = Some(err);
                None
            }
        }
    }
}

pub struct BlfWriter {
    file: File,
    compression_level: i32,
    max_container_size: usize,
    buffer: Vec<u8>,
    buffer_size: usize,
    object_count: u32,
    uncompressed_size: u64,
    start_timestamp: Option<f64>,
    stop_timestamp: Option<f64>,
    finished: bool,
}

impl BlfWriter {
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::create_with_level(path, -1)
    }

    pub fn create_with_level<P: AsRef<Path>>(path: P, compression_level: i32) -> Result<Self> {
        let file = File::create(path)?;
        let mut writer = Self {
            file,
            compression_level,
            max_container_size: 128 * 1024,
            buffer: Vec::new(),
            buffer_size: 0,
            object_count: 0,
            uncompressed_size: FILE_HEADER_SIZE as u64,
            start_timestamp: None,
            stop_timestamp: None,
            finished: false,
        };
        writer.write_header(FILE_HEADER_SIZE as u64)?;
        Ok(writer)
    }

    pub fn on_message_received(&mut self, msg: &Message) -> Result<()> {
        if msg.is_error_frame {
            self.write_can_error_ext(msg)
        } else if msg.is_fd {
            self.write_can_fd_message(msg)
        } else {
            self.write_can_message(msg)
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let uncompressed_len = std::cmp::min(self.buffer.len(), self.max_container_size);
        let uncompressed_data = self.buffer[..uncompressed_len].to_vec();
        let uncompressed_len_u32 = uncompressed_data.len() as u32;
        let tail = self.buffer.split_off(uncompressed_len);
        self.buffer = tail;
        self.buffer_size = self.buffer.len();

        let (method, data) = if self.compression_level == 0 {
            (NO_COMPRESSION, uncompressed_data)
        } else {
            let level = if self.compression_level < 0 {
                Compression::default()
            } else {
                Compression::new(self.compression_level as u32)
            };
            let mut encoder = ZlibEncoder::new(Vec::new(), level);
            encoder.write_all(&uncompressed_data)?;
            let compressed = encoder.finish()?;
            (ZLIB_DEFLATE, compressed)
        };

        let obj_size = (OBJ_HEADER_BASE_SIZE + LOG_CONTAINER_SIZE + data.len()) as u32;
        let mut header = Vec::with_capacity(OBJ_HEADER_BASE_SIZE + LOG_CONTAINER_SIZE);
        header.extend_from_slice(b"LOBJ");
        header.extend_from_slice(&(OBJ_HEADER_BASE_SIZE as u16).to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&obj_size.to_le_bytes());
        header.extend_from_slice(&LOG_CONTAINER.to_le_bytes());

        let mut container = Vec::with_capacity(LOG_CONTAINER_SIZE);
        container.extend_from_slice(&method.to_le_bytes());
        container.extend_from_slice(&[0u8; 6]);
        container.extend_from_slice(&uncompressed_len_u32.to_le_bytes());
        container.extend_from_slice(&[0u8; 4]);

        self.file.write_all(&header)?;
        self.file.write_all(&container)?;
        self.file.write_all(&data)?;
        let padding = (obj_size as usize) % 4;
        if padding != 0 {
            self.file.write_all(&vec![0u8; padding])?;
        }

        self.uncompressed_size += OBJ_HEADER_BASE_SIZE as u64;
        self.uncompressed_size += LOG_CONTAINER_SIZE as u64;
        self.uncompressed_size += uncompressed_len_u32 as u64;

        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush()?;
        let filesize = self.file.seek(SeekFrom::End(0))?;
        self.write_header(filesize)?;
        self.file.flush()?;
        self.finished = true;
        Ok(())
    }

    fn write_header(&mut self, filesize: u64) -> Result<()> {
        let mut buf = Vec::with_capacity(FILE_HEADER_SIZE as usize);
        buf.extend_from_slice(b"LOGG");
        buf.extend_from_slice(&FILE_HEADER_SIZE.to_le_bytes());
        buf.push(5); // application id
        buf.push(0);
        buf.push(0);
        buf.push(0);
        buf.push(2);
        buf.push(6);
        buf.push(8);
        buf.push(1);
        buf.extend_from_slice(&filesize.to_le_bytes());
        buf.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        buf.extend_from_slice(&self.object_count.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let start = timestamp_to_systemtime(self.start_timestamp);
        let stop = timestamp_to_systemtime(self.stop_timestamp);
        for v in start.iter() {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        for v in stop.iter() {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        if buf.len() < FILE_HEADER_SIZE as usize {
            buf.resize(FILE_HEADER_SIZE as usize, 0);
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&buf)?;
        Ok(())
    }

    fn add_object(&mut self, obj_type: u32, data: &[u8], timestamp: f64) -> Result<()> {
        if self.start_timestamp.is_none() {
            let start = (timestamp * 1000.0).trunc() / 1000.0;
            self.start_timestamp = Some(start);
        }
        self.stop_timestamp = Some(timestamp);
        let start = self.start_timestamp.unwrap();
        let mut rel = ((timestamp - start) * 1e9).trunc();
        if rel < 0.0 {
            rel = 0.0;
        }
        let rel = rel as u64;

        let header_size = (OBJ_HEADER_BASE_SIZE + OBJ_HEADER_V1_SIZE) as u16;
        let obj_size = (OBJ_HEADER_BASE_SIZE + OBJ_HEADER_V1_SIZE + data.len()) as u32;

        let mut header = Vec::with_capacity(OBJ_HEADER_BASE_SIZE + OBJ_HEADER_V1_SIZE);
        header.extend_from_slice(b"LOBJ");
        header.extend_from_slice(&header_size.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&obj_size.to_le_bytes());
        header.extend_from_slice(&obj_type.to_le_bytes());
        header.extend_from_slice(&TIME_ONE_NANS.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&rel.to_le_bytes());

        self.buffer.extend_from_slice(&header);
        self.buffer.extend_from_slice(data);
        let padding = data.len() % 4;
        if padding != 0 {
            self.buffer.extend_from_slice(&vec![0u8; padding]);
        }
        self.buffer_size += (obj_size as usize) + padding;
        self.object_count = self.object_count.wrapping_add(1);

        if self.buffer_size >= self.max_container_size {
            self.flush()?;
        }
        Ok(())
    }

    fn write_can_message(&mut self, msg: &Message) -> Result<()> {
        let channel = msg.channel.saturating_add(1);
        let mut arb_id = msg.arbitration_id;
        if msg.is_extended_id {
            arb_id |= CAN_MSG_EXT;
        }
        let mut flags = if msg.is_remote_frame { REMOTE_FLAG } else { 0 };
        if !msg.is_rx {
            flags |= DIR;
        }
        let dlc = msg.dlc;
        let mut data = [0u8; 8];
        let copy_len = std::cmp::min(msg.data.len(), 8);
        data[..copy_len].copy_from_slice(&msg.data[..copy_len]);

        let mut payload = Vec::with_capacity(CAN_MSG_SIZE);
        payload.extend_from_slice(&channel.to_le_bytes());
        payload.push(flags);
        payload.push(dlc);
        payload.extend_from_slice(&arb_id.to_le_bytes());
        payload.extend_from_slice(&data);
        self.add_object(CAN_MESSAGE, &payload, msg.timestamp)
    }

    fn write_can_fd_message(&mut self, msg: &Message) -> Result<()> {
        let channel = msg.channel.saturating_add(1);
        let mut arb_id = msg.arbitration_id;
        if msg.is_extended_id {
            arb_id |= CAN_MSG_EXT;
        }
        let mut flags = if msg.is_remote_frame { REMOTE_FLAG } else { 0 };
        if !msg.is_rx {
            flags |= DIR;
        }
        let dlc = len2dlc(msg.dlc);
        let mut fd_flags = EDL;
        if msg.bitrate_switch {
            fd_flags |= BRS;
        }
        if msg.error_state_indicator {
            fd_flags |= ESI;
        }
        let valid_bytes = std::cmp::min(msg.data.len(), 64) as u8;
        let mut data = [0u8; 64];
        data[..valid_bytes as usize].copy_from_slice(&msg.data[..valid_bytes as usize]);

        let mut payload = Vec::with_capacity(CAN_FD_MSG_SIZE);
        payload.extend_from_slice(&channel.to_le_bytes());
        payload.push(flags);
        payload.push(dlc);
        payload.extend_from_slice(&arb_id.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(0);
        payload.push(fd_flags);
        payload.push(valid_bytes);
        payload.extend_from_slice(&[0u8; 5]);
        payload.extend_from_slice(&data);
        self.add_object(CAN_FD_MESSAGE, &payload, msg.timestamp)
    }

    fn write_can_error_ext(&mut self, msg: &Message) -> Result<()> {
        let channel = msg.channel.saturating_add(1);
        let mut arb_id = msg.arbitration_id;
        if msg.is_extended_id {
            arb_id |= CAN_MSG_EXT;
        }
        let dlc = len2dlc(msg.dlc);
        let mut data = [0u8; 8];
        let copy_len = std::cmp::min(msg.data.len(), 8);
        data[..copy_len].copy_from_slice(&msg.data[..copy_len]);

        let mut payload = Vec::with_capacity(CAN_ERROR_EXT_SIZE);
        payload.extend_from_slice(&channel.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(0);
        payload.push(0);
        payload.push(dlc);
        payload.push(0);
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&arb_id.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&[0u8; 2]);
        payload.extend_from_slice(&data);
        self.add_object(CAN_ERROR_EXT, &payload, msg.timestamp)
    }
}

impl Drop for BlfWriter {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

// =========================
// ASC (Vector) Reader/Writer
// =========================

pub struct AscReader {
    reader: BufReader<File>,
    base: u32,
    relative_timestamp: bool,
    start_time: f64,
    header_parsed: bool,
    pending_line: Option<String>,
    error: Option<BlfError>,
}

impl AscReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_options(path, "hex", true)
    }

    pub fn open_with_options<P: AsRef<Path>>(
        path: P,
        base: &str,
        relative_timestamp: bool,
    ) -> Result<Self> {
        let file = File::open(path)?;
        let base = Self::check_base(base)?;
        Ok(Self {
            reader: BufReader::new(file),
            base,
            relative_timestamp,
            start_time: 0.0,
            header_parsed: false,
            pending_line: None,
            error: None,
        })
    }

    pub fn take_error(&mut self) -> Option<BlfError> {
        self.error.take()
    }

    fn read_line(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }

    fn extract_header(&mut self) -> Result<()> {
        while let Some(line) = self.read_line()? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if lower.starts_with("date ") {
                let datetime_string = trimmed.splitn(3, ' ').skip(2).collect::<Vec<_>>().join(" ");
                if !self.relative_timestamp {
                    if let Some(ts) = Self::datetime_to_timestamp(&datetime_string) {
                        self.start_time = ts;
                    }
                }
                continue;
            }
            if lower.starts_with("base ") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(base) = Self::check_base(parts[1]) {
                        self.base = base;
                    }
                }
                continue;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            if lower.contains("internal events logged") {
                break;
            }
            self.pending_line = Some(line);
            break;
        }
        self.header_parsed = true;
        Ok(())
    }

    fn check_base(base: &str) -> Result<u32> {
        match base {
            "hex" | "HEX" | "Hex" => Ok(16),
            "dec" | "DEC" | "Dec" => Ok(10),
            _ => Err(BlfError::Parse("base should be either hex or dec".into())),
        }
    }

    fn datetime_to_timestamp(datetime_string: &str) -> Option<f64> {
        let parts: Vec<&str> = datetime_string.split_whitespace().collect();
        if parts.len() < 4 {
            return None;
        }

        let month = Self::parse_month(parts[0])?;
        let day: u32 = parts[1].parse().ok()?;
        let time_str = parts[2];

        let mut year_idx = 3;
        let mut am_pm: Option<&str> = None;
        if parts.len() >= 5 && (parts[3].eq_ignore_ascii_case("AM") || parts[3].eq_ignore_ascii_case("PM")) {
            am_pm = Some(parts[3]);
            year_idx = 4;
        }
        let year: i32 = parts.get(year_idx)?.parse().ok()?;

        let (mut hour, minute, second, nanos) = Self::parse_time(time_str)?;
        if let Some(meridian) = am_pm {
            let is_pm = meridian.eq_ignore_ascii_case("PM");
            if hour == 12 {
                hour = if is_pm { 12 } else { 0 };
            } else if is_pm {
                hour += 12;
            }
        }

        let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
        let time = chrono::NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)?;
        let dt = chrono::NaiveDateTime::new(date, time);
        let local_dt = Local.from_local_datetime(&dt).single()?;
        Some(local_dt.timestamp() as f64 + (local_dt.timestamp_subsec_nanos() as f64 / 1e9))
    }

    fn parse_month(s: &str) -> Option<u32> {
        let lower = s.to_lowercase();
        let map = [
            ("jan", 1),
            ("feb", 2),
            ("mar", 3),
            ("apr", 4),
            ("may", 5),
            ("jun", 6),
            ("jul", 7),
            ("aug", 8),
            ("sep", 9),
            ("oct", 10),
            ("nov", 11),
            ("dec", 12),
            ("mär", 3),
            ("mai", 5),
            ("okt", 10),
            ("dez", 12),
        ];
        for (name, val) in map.iter() {
            if lower.starts_with(name) {
                return Some(*val);
            }
        }
        if let Ok(v) = s.parse::<u32>() {
            return Some(v);
        }
        None
    }

    fn parse_time(s: &str) -> Option<(u32, u32, u32, u32)> {
        let mut iter = s.split(':');
        let hour: u32 = iter.next()?.parse().ok()?;
        let minute: u32 = iter.next()?.parse().ok()?;
        let sec_str = iter.next()?;
        let (second, nanos) = if let Some((sec_part, frac_part)) = sec_str.split_once('.') {
            let second: u32 = sec_part.parse().ok()?;
            let mut frac = frac_part.chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
            if frac.len() > 9 {
                frac.truncate(9);
            }
            while frac.len() < 9 {
                frac.push('0');
            }
            let nanos: u32 = frac.parse().ok()?;
            (second, nanos)
        } else {
            let second: u32 = sec_str.parse().ok()?;
            (second, 0)
        };
        Some((hour, minute, second, nanos))
    }

    fn parse_can_id(&self, s: &str) -> Result<(u32, bool)> {
        let mut is_extended = false;
        let mut id_str = s.to_string();
        if let Some(last) = s.chars().last() {
            if last == 'x' || last == 'X' {
                is_extended = true;
                id_str.pop();
            }
        }
        let can_id = u32::from_str_radix(id_str.trim(), self.base)
            .map_err(|_| BlfError::Parse("invalid CAN id".into()))?;
        Ok((can_id, is_extended))
    }

    fn parse_data_tokens(&self, tokens: &[&str], count: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(count);
        for tok in tokens.iter().take(count) {
            if let Ok(val) = u8::from_str_radix(tok, self.base) {
                data.push(val);
            } else {
                break;
            }
        }
        data
    }

    pub fn next_message(&mut self) -> Result<Option<Message>> {
        if !self.header_parsed {
            self.extract_header()?;
        }
        loop {
            let line = if let Some(pending) = self.pending_line.take() {
                pending
            } else {
                match self.read_line()? {
                    Some(l) => l,
                    None => return Ok(None),
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if lower.starts_with("begin triggerblock") {
                if self.relative_timestamp {
                    self.start_time = 0.0;
                } else {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 4 {
                        let datetime_string = parts[3..].join(" ");
                        if let Some(ts) = Self::datetime_to_timestamp(&datetime_string) {
                            self.start_time = ts;
                        }
                    }
                }
                continue;
            }
            if lower.starts_with("end triggerblock") {
                continue;
            }
            if lower.contains("start of measurement") {
                continue;
            }

            let tokens: Vec<&str> = trimmed.split_whitespace().collect();
            if tokens.len() < 2 {
                continue;
            }
            let timestamp = match tokens[0].parse::<f64>() {
                Ok(t) => t + self.start_time,
                Err(_) => continue,
            };

            if tokens[1].eq_ignore_ascii_case("canfd") {
                if tokens.len() < 5 {
                    continue;
                }
                let channel = match tokens[2].parse::<u16>() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let direction = tokens[3];
                let rest = &tokens[4..];
                if rest.is_empty() {
                    continue;
                }
                if rest[0].eq_ignore_ascii_case("errorframe") {
                    return Ok(Some(Message {
                        timestamp,
                        is_fd: true,
                        is_error_frame: true,
                        is_rx: direction.eq_ignore_ascii_case("Rx"),
                        channel: channel.saturating_sub(1),
                        ..Default::default()
                    }));
                }

                let (can_id, is_extended) = self.parse_can_id(rest[0])?;
                let mut idx = 1;
                let mut brs_str = rest.get(idx).copied().unwrap_or("");
                let mut symbolic_name_skipped = false;
                if brs_str.chars().all(|c| c.is_ascii_digit()) {
                    // no symbolic name
                } else {
                    symbolic_name_skipped = true;
                    idx += 1;
                    brs_str = rest.get(idx).copied().unwrap_or("");
                }
                idx += 1;
                let esi_str = rest.get(idx).copied().unwrap_or("0");
                idx += 1;
                let dlc_str = rest.get(idx).copied().unwrap_or("0");
                idx += 1;
                let data_length_str = rest.get(idx).copied().unwrap_or("0");
                idx += 1;

                let brs = brs_str == "1";
                let esi = esi_str == "1";
                let dlc = u8::from_str_radix(dlc_str, self.base).unwrap_or(0);
                let data_length = data_length_str.parse::<usize>().unwrap_or(0);
                let data_tokens = if symbolic_name_skipped {
                    &rest[idx..]
                } else {
                    &rest[idx..]
                };
                let data = self.parse_data_tokens(data_tokens, data_length);

                let mut msg = Message {
                    timestamp,
                    arbitration_id: can_id,
                    is_extended_id: is_extended,
                    is_remote_frame: data_length == 0,
                    is_rx: direction.eq_ignore_ascii_case("Rx"),
                    is_error_frame: false,
                    is_fd: true,
                    bitrate_switch: brs,
                    error_state_indicator: esi,
                    dlc: if data_length == 0 { dlc } else { data_length as u8 },
                    data,
                    channel: channel.saturating_sub(1),
                };

                if data_length == 0 {
                    msg.is_remote_frame = true;
                }

                return Ok(Some(msg));
            }

            if tokens[1].chars().all(|c| c.is_ascii_digit()) {
                if tokens.len() < 3 {
                    continue;
                }
                let channel = match tokens[1].parse::<u16>() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if tokens[2].eq_ignore_ascii_case("errorframe") {
                    return Ok(Some(Message {
                        timestamp,
                        is_error_frame: true,
                        is_rx: true,
                        channel: channel.saturating_sub(1),
                        ..Default::default()
                    }));
                }
                if tokens.len() < 6 {
                    continue;
                }
                let (can_id, is_extended) = self.parse_can_id(tokens[2])?;
                let direction = tokens[3];
                let dtype = tokens[4];
                if dtype.eq_ignore_ascii_case("r") {
                    let dlc = tokens.get(5).and_then(|v| u8::from_str_radix(v, self.base).ok());
                    return Ok(Some(Message {
                        timestamp,
                        arbitration_id: can_id,
                        is_extended_id: is_extended,
                        is_remote_frame: true,
                        is_rx: direction.eq_ignore_ascii_case("Rx"),
                        is_error_frame: false,
                        is_fd: false,
                        bitrate_switch: false,
                        error_state_indicator: false,
                        dlc: dlc.unwrap_or(0),
                        data: Vec::new(),
                        channel: channel.saturating_sub(1),
                    }));
                }
                if dtype.eq_ignore_ascii_case("d") {
                    let dlc_str = tokens.get(5).copied().unwrap_or("0");
                    let dlc_code = u8::from_str_radix(dlc_str, self.base).unwrap_or(0);
                    let dlc = dlc2len(dlc_code);
                    let data_tokens = if tokens.len() > 6 { &tokens[6..] } else { &[] };
                    let data = self.parse_data_tokens(data_tokens, std::cmp::min(8, dlc as usize));
                    return Ok(Some(Message {
                        timestamp,
                        arbitration_id: can_id,
                        is_extended_id: is_extended,
                        is_remote_frame: false,
                        is_rx: direction.eq_ignore_ascii_case("Rx"),
                        is_error_frame: false,
                        is_fd: false,
                        bitrate_switch: false,
                        error_state_indicator: false,
                        dlc,
                        data,
                        channel: channel.saturating_sub(1),
                    }));
                }
            }
        }
    }
}

impl Iterator for AscReader {
    type Item = Message;

    fn next(&mut self) -> Option<Self::Item> {
        if self.error.is_some() {
            return None;
        }
        match self.next_message() {
            Ok(Some(msg)) => Some(msg),
            Ok(None) => None,
            Err(err) => {
                self.error = Some(err);
                None
            }
        }
    }
}

pub struct AscWriter {
    writer: BufWriter<File>,
    channel: u16,
    header_written: bool,
    last_timestamp: f64,
    started: f64,
    finished: bool,
}

impl AscWriter {
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::create_with_channel(path, 1)
    }

    pub fn create_with_channel<P: AsRef<Path>>(path: P, channel: u16) -> Result<Self> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        let start_time = Self::format_header_datetime(Local::now());
        writeln!(writer, "date {}", start_time)?;
        writeln!(writer, "base hex  timestamps absolute")?;
        writeln!(writer, "internal events logged")?;
        Ok(Self {
            writer,
            channel,
            header_written: false,
            last_timestamp: 0.0,
            started: 0.0,
            finished: false,
        })
    }

    pub fn on_message_received(&mut self, msg: &Message) -> Result<()> {
        let mut channel = msg.channel.saturating_add(1);
        if channel == 0 {
            channel = self.channel;
        }

        if msg.is_error_frame {
            self.log_event(&format!("{channel}  ErrorFrame"), msg.timestamp)?;
            return Ok(());
        }

        let data_str = if msg.is_remote_frame {
            "".to_string()
        } else {
            msg.data
                .iter()
                .map(|b| format!("{:02X}", b))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let arb_id = if msg.is_extended_id {
            format!("{:X}x", msg.arbitration_id)
        } else {
            format!("{:X}", msg.arbitration_id)
        };

        if msg.is_fd {
            let flags = (1 << 12)
                | if msg.bitrate_switch { 1 << 13 } else { 0 }
                | if msg.error_state_indicator { 1 << 14 } else { 0 };
            let dlc = len2dlc(msg.dlc);
            let data_length = if msg.is_remote_frame { 0 } else { msg.data.len() };
            let serialized = format!(
                "CANFD {:>3} {:<4} {:>8}  {:>32} {} {} {:x} {:>2} {} {:>8} {:>4} {:>8X} {:>8} {:>8} {:>8} {:>8} {:>8}",
                channel,
                if msg.is_rx { "Rx" } else { "Tx" },
                arb_id,
                "",
                if msg.bitrate_switch { 1 } else { 0 },
                if msg.error_state_indicator { 1 } else { 0 },
                dlc,
                data_length,
                data_str,
                0,
                0,
                flags,
                0,
                0,
                0,
                0,
                0,
            );
            self.log_event(&serialized, msg.timestamp)?;
        } else {
            let dtype = if msg.is_remote_frame {
                format!("r {:x}", msg.dlc)
            } else {
                format!("d {:x}", msg.dlc)
            };
            let serialized = format!(
                "{channel}  {id:<15} {dir:<4} {dtype} {data}",
                channel = channel,
                id = arb_id,
                dir = if msg.is_rx { "Rx" } else { "Tx" },
                dtype = dtype,
                data = data_str,
            );
            self.log_event(&serialized, msg.timestamp)?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        writeln!(self.writer, "End TriggerBlock")?;
        self.writer.flush()?;
        self.finished = true;
        Ok(())
    }

    fn log_event(&mut self, message: &str, timestamp: f64) -> Result<()> {
        if !self.header_written {
            self.started = timestamp;
            self.last_timestamp = timestamp;
            let start_time = Self::format_header_datetime(Self::datetime_from_timestamp(timestamp));
            writeln!(self.writer, "Begin Triggerblock {}", start_time)?;
            self.header_written = true;
            self.log_event("Start of measurement", timestamp)?;
        }
        let mut ts = timestamp;
        if ts == 0.0 {
            ts = self.last_timestamp;
        }
        if ts >= self.started {
            ts -= self.started;
        }
        self.last_timestamp = timestamp;
        writeln!(self.writer, "{ts:9.6} {message}")?;
        Ok(())
    }

    fn format_header_datetime(dt: chrono::DateTime<Local>) -> String {
        let msec = dt.timestamp_subsec_millis();
        format!("{}.{} {}", dt.format("%a %b %d %H:%M:%S"), format!("{:03}", msec), dt.format("%Y"))
    }

    fn datetime_from_timestamp(timestamp: f64) -> chrono::DateTime<Local> {
        let secs = timestamp.floor() as i64;
        let nanos = ((timestamp - secs as f64) * 1e9).round() as u32;
        Local
            .timestamp_opt(secs, nanos)
            .single()
            .unwrap_or_else(Local::now)
    }
}

impl Drop for AscWriter {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
