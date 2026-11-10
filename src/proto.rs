//! Wire protocol: a Noise_NNpsk0 handshake (pre-shared key = authentication,
//! fresh ephemeral keys = forward secrecy), then length-prefixed encrypted
//! postcard messages.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::geometry::Side;

pub const PROTOCOL_VERSION: u32 = 1;
const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const MAX_FRAME: usize = 65535;
/// Keep plaintext well under the Noise limit (65535 incl. 16-byte tag).
pub const MAX_PAYLOAD: usize = 60000;

/// One evdev event: (type, code, value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ev {
    pub t: u16,
    pub c: u16,
    pub v: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsAxis {
    pub code: u16,
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub res: i32,
}

/// Everything needed to recreate an input device on the other machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDesc {
    pub name: String,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub props: Vec<u16>,
    pub keys: Vec<u16>,
    pub rel: Vec<u16>,
    pub abs: Vec<AbsAxis>,
    pub msc: Vec<u16>,
    pub pointer: bool,
    pub touchpad: bool,
    pub keyboard: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    Hello {
        name: String,
        version: u32,
    },
    Ping,
    // controller → receiver
    AddDevice {
        id: u32,
        desc: DeviceDesc,
    },
    RemoveDevice {
        id: u32,
    },
    /// Take over: put the cursor on your `side` edge at `frac` along it.
    Enter {
        side: Side,
        frac: f64,
    },
    Input {
        id: u32,
        events: Vec<Ev>,
    },
    /// Controller gave up control (release keys, disconnect, …).
    Leave,
    // receiver → controller
    /// The cursor left through the edge facing you, at `frac` along it.
    Released {
        frac: f64,
    },
}

fn noise_builder(psk: &[u8; 32]) -> Result<snow::Builder<'_>> {
    Ok(snow::Builder::new(NOISE_PARAMS.parse()?).psk(0, psk)?)
}

async fn write_frame(w: &mut OwnedWriteHalf, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).context("frame too large")?;
    let mut buf = Vec::with_capacity(2 + data.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf).await?;
    Ok(())
}

async fn read_frame(r: &mut OwnedReadHalf, buf: &mut Vec<u8>) -> Result<()> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let len = u16::from_be_bytes(len) as usize;
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    Ok(())
}

pub struct Reader {
    half: OwnedReadHalf,
    noise: Arc<Mutex<snow::TransportState>>,
    frame: Vec<u8>,
    plain: Vec<u8>,
}

pub struct Writer {
    half: OwnedWriteHalf,
    noise: Arc<Mutex<snow::TransportState>>,
    cipher: Vec<u8>,
}

impl Reader {
    pub async fn recv(&mut self) -> Result<Msg> {
        read_frame(&mut self.half, &mut self.frame).await?;
        self.plain.resize(MAX_FRAME, 0);
        let n = self
            .noise
            .lock()
            .unwrap()
            .read_message(&self.frame, &mut self.plain)
            .context("decrypt (wrong psk or tampered data)")?;
        Ok(postcard::from_bytes(&self.plain[..n])?)
    }
}

impl Writer {
    pub async fn send(&mut self, msg: &Msg) -> Result<()> {
        let plain = postcard::to_allocvec(msg)?;
        if plain.len() > MAX_PAYLOAD {
            bail!("message too large ({} bytes)", plain.len());
        }
        self.cipher.resize(MAX_FRAME, 0);
        let n = self
            .noise
            .lock()
            .unwrap()
            .write_message(&plain, &mut self.cipher)?;
        let frame = std::mem::take(&mut self.cipher);
        let res = write_frame(&mut self.half, &frame[..n]).await;
        self.cipher = frame;
        res
    }
}

/// Run the handshake on a fresh TCP connection.
pub async fn handshake(
    stream: TcpStream,
    psk: &[u8; 32],
    initiator: bool,
) -> Result<(Reader, Writer)> {
    stream.set_nodelay(true)?;
    let (mut r, mut w) = stream.into_split();
    let builder = noise_builder(psk)?;
    let mut hs = if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    };
    let mut msg = vec![0u8; MAX_FRAME];
    let mut frame = Vec::new();
    let mut scratch = vec![0u8; MAX_FRAME];
    if initiator {
        let n = hs.write_message(&[], &mut msg)?;
        write_frame(&mut w, &msg[..n]).await?;
        read_frame(&mut r, &mut frame).await?;
        hs.read_message(&frame, &mut scratch)
            .context("handshake failed (psk mismatch?)")?;
    } else {
        read_frame(&mut r, &mut frame).await?;
        hs.read_message(&frame, &mut scratch)
            .context("handshake failed (psk mismatch?)")?;
        let n = hs.write_message(&[], &mut msg)?;
        write_frame(&mut w, &msg[..n]).await?;
    }
    let noise = Arc::new(Mutex::new(hs.into_transport_mode()?));
    Ok((
        Reader {
            half: r,
            noise: noise.clone(),
            frame: Vec::new(),
            plain: Vec::new(),
        },
        Writer {
            half: w,
            noise,
            cipher: Vec::new(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn roundtrip_over_noise() {
        let psk = [7u8; 32];
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let (mut r, mut w) = handshake(s, &psk, false).await.unwrap();
            let m = r.recv().await.unwrap();
            w.send(&m).await.unwrap();
        });
        let s = TcpStream::connect(addr).await.unwrap();
        let (mut r, mut w) = handshake(s, &psk, true).await.unwrap();
        let msg = Msg::Input {
            id: 3,
            events: vec![Ev { t: 2, c: 0, v: -5 }, Ev { t: 1, c: 30, v: 1 }],
        };
        w.send(&msg).await.unwrap();
        assert_eq!(r.recv().await.unwrap(), msg);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_psk_is_rejected() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            handshake(s, &[1u8; 32], false).await.is_err()
        });
        let s = TcpStream::connect(addr).await.unwrap();
        let _ = handshake(s, &[2u8; 32], true).await;
        assert!(server.await.unwrap());
    }
}
