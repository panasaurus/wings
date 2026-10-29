use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

pub const HANDSHAKE_LIMIT: usize = 2048;
const STATUS_LIMIT: usize = 64 * 1024;

pub fn varint(mut value: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        bytes.push(byte | if value == 0 { 0 } else { 128 });
        if value == 0 {
            return bytes;
        }
    }
}

fn number(bytes: &[u8], cursor: &mut usize) -> Result<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*cursor).context("Truncated VarInt")?;
        *cursor += 1;
        ensure!(shift < 28 || byte & 0xf0 == 0, "VarInt overflow");
        value |= u32::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("VarInt too long")
}

fn string<'a>(bytes: &'a [u8], cursor: &mut usize, limit: usize) -> Result<&'a str> {
    let length = number(bytes, cursor)? as usize;
    ensure!(length <= limit * 4, "String too long");
    let end = cursor.checked_add(length).context("String overflow")?;
    let value = std::str::from_utf8(bytes.get(*cursor..end).context("Truncated string")?)?;
    ensure!(value.chars().count() <= limit, "String too long");
    *cursor = end;
    Ok(value)
}

pub async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R, limit: usize) -> Result<Vec<u8>> {
    let mut prefix = Vec::new();
    for _ in 0..5 {
        let byte = stream.read_u8().await?;
        prefix.push(byte);
        if byte & 128 == 0 {
            break;
        }
    }
    let length = number(&prefix, &mut 0)? as usize;
    ensure!(length > 0 && length <= limit, "Invalid packet length");
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(stream: &mut W, frame: &[u8]) -> Result<()> {
    stream.write_all(&varint(frame.len() as u32)).await?;
    stream.write_all(frame).await?;
    Ok(())
}

pub struct Handshake {
    pub protocol: i32,
    pub next_state: u32,
}

pub fn handshake(frame: &[u8]) -> Result<Handshake> {
    let mut cursor = 0;
    ensure!(number(frame, &mut cursor)? == 0, "Expected handshake");
    let protocol = number(frame, &mut cursor)? as i32;
    let _host = string(frame, &mut cursor, 255)?;
    cursor = cursor.checked_add(2).context("Port overflow")?;
    ensure!(cursor <= frame.len(), "Missing port");
    let next_state = number(frame, &mut cursor)?;
    ensure!(
        cursor == frame.len() && matches!(next_state, 1 | 2),
        "Unsupported handshake state"
    );
    Ok(Handshake {
        protocol,
        next_state,
    })
}

pub fn login_start(frame: &[u8]) -> Result<()> {
    let mut cursor = 0;
    ensure!(number(frame, &mut cursor)? == 0, "Expected Login Start");
    let name = string(frame, &mut cursor, 16)?;
    ensure!(
        !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "Invalid Minecraft username"
    );
    Ok(())
}

pub fn decode_status(frame: &[u8]) -> Result<Value> {
    let mut cursor = 0;
    ensure!(number(frame, &mut cursor)? == 0, "Expected status response");
    let text = string(frame, &mut cursor, STATUS_LIMIT / 4)?;
    ensure!(cursor == frame.len(), "Trailing status data");
    let value: Value = serde_json::from_str(text)?;
    let online = value["players"]["online"]
        .as_u64()
        .context("Unknown player count")?;
    let max = value["players"]["max"]
        .as_u64()
        .context("Missing player capacity")?;
    ensure!(
        online <= 1_000_000 && max <= 1_000_000,
        "Invalid player count"
    );
    ensure!(
        value["version"]["protocol"].as_i64().is_some(),
        "Missing protocol version"
    );
    Ok(value)
}

pub async fn query(address: SocketAddr) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect(address).await?;
        let host = "localhost";
        let mut hello = vec![0];
        hello.extend(varint(u32::MAX));
        hello.extend(varint(host.len() as u32));
        hello.extend(host.as_bytes());
        hello.extend(address.port().to_be_bytes());
        hello.push(1);
        write_frame(&mut stream, &hello).await?;
        write_frame(&mut stream, &[0]).await?;
        let mut ping = vec![1];
        ping.extend(0i64.to_be_bytes());
        write_frame(&mut stream, &ping).await?;
        decode_status(&read_frame(&mut stream, STATUS_LIMIT).await?)
    })
    .await
    .context("Minecraft status timed out")?
}

pub fn unavailable(cached: Option<&Value>, protocol: i32, message: &str) -> Value {
    json!({"version": {"name": cached.and_then(|v| v["version"]["name"].as_str()).unwrap_or("Minecraft"),
        "protocol": cached.and_then(|v| v["version"]["protocol"].as_i64()).unwrap_or(i64::from(protocol))},
        "players": {"max":cached.and_then(|v| v["players"]["max"].as_u64()).unwrap_or(20), "online":0,"sample":[]},
        "description":{"text":message}})
}

pub async fn respond(stream: &mut TcpStream, value: &Value) -> Result<()> {
    ensure!(
        read_frame(stream, 16).await? == [0],
        "Expected status request"
    );
    let text = serde_json::to_vec(value)?;
    ensure!(text.len() < STATUS_LIMIT - 8, "Status too large");
    let mut response = vec![0];
    response.extend(varint(text.len() as u32));
    response.extend(text);
    write_frame(stream, &response).await?;
    let ping = read_frame(stream, 16).await?;
    ensure!(ping.len() == 9 && ping[0] == 1, "Expected status ping");
    write_frame(stream, &ping).await
}
