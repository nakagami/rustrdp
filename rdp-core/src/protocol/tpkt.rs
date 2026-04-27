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
        self.transport.send(&buf).await
    }

    pub async fn recv(&mut self) -> Result<(bool, Vec<u8>), RdpError> {
        let first = self.transport.recv_exact(1).await?;
        let b0 = first[0];

        if (b0 & 0x03) == 0x03 {
            let rest = self.transport.recv_exact(3).await?;
            let len = ((rest[1] as usize) << 8) | (rest[2] as usize);
            if len < 4 {
                return Err(RdpError::Protocol("TPKT length too short".into()));
            }
            let payload = self.transport.recv_exact(len - 4).await?;
            Ok((false, payload))
        } else {
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
