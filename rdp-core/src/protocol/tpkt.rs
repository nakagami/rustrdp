use crate::error::RdpError;
use crate::protocol::Transport;

pub struct Tpkt<T: Transport> {
    transport: T,
    recv_buf: Vec<u8>,
}

impl<T: Transport> Tpkt<T> {
    pub fn new(transport: T) -> Self {
        Tpkt {
            transport,
            recv_buf: Vec::new(),
        }
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<(), RdpError> {
        let len = (data.len() + 4) as u16;
        let mut buf = vec![0x03, 0x00, (len >> 8) as u8, len as u8];
        buf.extend_from_slice(data);

        #[cfg(debug_assertions)]
        eprintln!("[TPKT] send total={} hex={}", buf.len(), hex_dump(&buf));

        self.transport.send(&buf).await
    }

    pub async fn recv(&mut self) -> Result<(bool, Vec<u8>), RdpError> {
        self.ensure_recv_buf(1).await?;
        let b0 = self.recv_buf[0];

        if (b0 & 0x03) == 0x03 {
            // Standard TPKT
            self.ensure_recv_buf(4).await?;
            let len = ((self.recv_buf[2] as usize) << 8) | (self.recv_buf[3] as usize);
            if len < 4 {
                self.recv_buf.clear();
                return Err(RdpError::Protocol("TPKT length too short".into()));
            }
            self.ensure_recv_buf(len).await?;
            let payload = self.recv_buf[4..len].to_vec();

            #[cfg(debug_assertions)]
            {
                let full = &self.recv_buf[..len];
                eprintln!("[TPKT] recv total={} hex={}", full.len(), hex_dump(full));
            }

            self.recv_buf.drain(..len);
            Ok((false, payload))
        } else {
            // FastPath
            self.ensure_recv_buf(2).await?;
            let b1 = self.recv_buf[1];
            let (length, extra) = if b1 & 0x80 != 0 {
                self.ensure_recv_buf(3).await?;
                let b2 = self.recv_buf[2];
                let len = (((b1 & 0x7F) as usize) << 8) | (b2 as usize);
                (len, 3usize)
            } else {
                (b1 as usize, 2usize)
            };
            if length < extra {
                self.recv_buf.clear();
                return Err(RdpError::Protocol("FastPath length too short".into()));
            }
            self.ensure_recv_buf(length).await?;
            let payload = self.recv_buf[extra..length].to_vec();

            #[cfg(debug_assertions)]
            eprintln!("[TPKT] recv FastPath total={}", length);

            self.recv_buf.drain(..length);
            Ok((true, payload))
        }
    }

    async fn ensure_recv_buf(&mut self, n: usize) -> Result<(), RdpError> {
        while self.recv_buf.len() < n {
            let need = n - self.recv_buf.len();
            let chunk = self.transport.recv_exact(need).await?;
            self.recv_buf.extend_from_slice(&chunk);
        }
        Ok(())
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }
}

#[cfg(debug_assertions)]
fn hex_dump(data: &[u8]) -> String {
    let limit = data.len().min(64);
    let hex: Vec<String> = data[..limit].iter().map(|b| format!("{:02x}", b)).collect();
    if data.len() > limit {
        format!("{}...({}bytes)", hex.join(" "), data.len())
    } else {
        hex.join(" ")
    }
}
