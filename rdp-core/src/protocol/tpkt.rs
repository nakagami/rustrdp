use crate::error::RdpError;
use crate::protocol::Transport;

pub struct Tpkt<T: Transport> {
    transport: T,
}

impl<T: Transport> Tpkt<T> {
    pub fn new(transport: T) -> Self {
        Tpkt { transport }
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
        let first = self.transport.recv_exact(1).await?;
        let b0 = first[0];

        if (b0 & 0x03) == 0x03 {
            // Standard TPKT
            let rest = self.transport.recv_exact(3).await?;
            let len = ((rest[1] as usize) << 8) | (rest[2] as usize);
            if len < 4 {
                return Err(RdpError::Protocol("TPKT length too short".into()));
            }
            let payload = self.transport.recv_exact(len - 4).await?;

            #[cfg(debug_assertions)]
            {
                let mut full = vec![b0];
                full.extend_from_slice(&rest);
                full.extend_from_slice(&payload);
                eprintln!("[TPKT] recv total={} hex={}", full.len(), hex_dump(&full));
            }

            Ok((false, payload))
        } else {
            // FastPath
            let b1 = self.transport.recv_exact(1).await?[0];
            let (length, extra) = if b1 & 0x80 != 0 {
                let b2 = self.transport.recv_exact(1).await?[0];
                let len = (((b1 & 0x7F) as usize) << 8) | (b2 as usize);
                (len, 3usize)
            } else {
                (b1 as usize, 2usize)
            };
            if length < extra {
                return Err(RdpError::Protocol("FastPath length too short".into()));
            }
            let payload = self.transport.recv_exact(length - extra).await?;

            #[cfg(debug_assertions)]
            eprintln!("[TPKT] recv FastPath total={}", length);

            Ok((true, payload))
        }
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
