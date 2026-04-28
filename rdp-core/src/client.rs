use crate::error::RdpError;
use crate::bitmap::Bitmap;
use crate::protocol::Transport;
use crate::protocol::tpkt::Tpkt;
use crate::protocol::x224::{X224, PROTOCOL_HYBRID, PROTOCOL_SSL};
use crate::protocol::t125::mcs::McsClient;
use crate::protocol::t125::gcc::{ClientData, create_gcc_data};
use crate::protocol::nla::cssp::Cssp;
use crate::protocol::lic::parse_license_pdu;
use crate::protocol::pdu::{
    ShareControlHeader, ShareDataHeader,
    PDUTYPE_DEMANDACTIVEPDU, PDUTYPE_CONFIRMACTIVEPDU, PDUTYPE_DEACTIVATEALLPDU, PDUTYPE_DATAPDU,
    PDUTYPE2_UPDATE, PDUTYPE2_CONTROL, PDUTYPE2_SYNCHRONIZE, PDUTYPE2_INPUT,
    PDUTYPE2_FONTMAP, PDUTYPE2_FONTLIST,
    UPDATETYPE_BITMAP,
};
use crate::protocol::pdu::caps::build_all_capabilities;
use crate::protocol::pdu::input::{
    build_keyboard_event, build_mouse_event, wrap_input_pdu,
    KBDFLAGS_KEYUP, PTRFLAGS_BUTTON1, PTRFLAGS_BUTTON2, PTRFLAGS_BUTTON3,
    PTRFLAGS_DOWN, PTRFLAGS_MOVE, PTRFLAGS_WHEEL, PTRFLAGS_WHEEL_NEGATIVE,
};
use crate::core::io::*;
use crate::core::rle;
use crate::protocol::nla::ntlm::to_utf16_le;

const BITMAP_COMPRESSION: u16 = 0x0001;
const NO_BITMAP_COMPRESSION_HDR: u16 = 0x0400;

pub enum RdpEvent {
    Ready,
    Bitmap(Vec<Bitmap>),
    Deactivated,
}

pub struct RdpSession<T: Transport> {
    mcs: McsClient<T>,
    share_id: u32,
    io_channel: u16,
    user_channel: u16,
    width: u16,
    height: u16,
    kbd_layout: u32,
}

impl<T: Transport> RdpSession<T> {
    /// Perform full RDP login and return a ready session.
    pub async fn login(
        transport: T,
        domain: &str,
        user: &str,
        password: &str,
        width: u16,
        height: u16,
        kbd_layout: u32,
    ) -> Result<Self, RdpError> {
        // Step 1: X.224 connection negotiation
        let tpkt = Tpkt::new(transport);
        let mut x224 = X224::new(tpkt);
        #[cfg(debug_assertions)]
        eprintln!("[client] step1: X.224 connect");
        x224.connect(user).await?;
        let selected_protocol = x224.recv_confirm().await?;
        #[cfg(debug_assertions)]
        eprintln!("[client] step1 done: selected_protocol=0x{:08x}", selected_protocol);

        // Step 2: TLS upgrade (proxy performs TLS and sends back the server public key)
        let pub_key = if selected_protocol == PROTOCOL_SSL || selected_protocol == PROTOCOL_HYBRID {
            #[cfg(debug_assertions)]
            eprintln!("[client] step2: start_tls");
            let pk = x224.tpkt_mut().transport_mut().start_tls().await?;
            #[cfg(debug_assertions)]
            eprintln!("[client] step2 done: pub_key len={}", pk.len());
            pk
        } else {
            Vec::new()
        };

        // Step 3: CredSSP / NLA authentication
        let transport = if selected_protocol == PROTOCOL_HYBRID {
            #[cfg(debug_assertions)]
            eprintln!("[client] step3: CredSSP/NLA authenticate");
            let inner_transport = x224.into_tpkt().into_transport();
            let mut cssp = Cssp::new(inner_transport, domain, user, password);
            cssp.authenticate(&pub_key).await?;
            #[cfg(debug_assertions)]
            eprintln!("[client] step3 done: CredSSP complete");
            cssp.into_transport()
        } else {
            x224.into_tpkt().into_transport()
        };

        // Step 4: Re-wrap transport (after CredSSP the TLS stream is reused)
        let tpkt = Tpkt::new(transport);
        let mut x224 = X224::new(tpkt);
        x224.selected_protocol = selected_protocol;

        // Step 5: MCS / GCC connection
        #[cfg(debug_assertions)]
        eprintln!("[client] step5: MCS connect");
        let client_data = ClientData {
            width,
            height,
            kbd_layout,
            color_depth: 0xca01,
            channels: vec![],
            server_selected_protocol: selected_protocol,
        };
        let gcc_data = create_gcc_data(&client_data);
        let mut mcs = McsClient::new(x224);
        mcs.connect(&gcc_data).await?;
        let server_data = mcs.recv_connect_response().await?;
        mcs.io_channel = server_data.io_channel;
        #[cfg(debug_assertions)]
        eprintln!("[client] step5 done: io_channel={} channels={:?}", mcs.io_channel, server_data.channels);
        mcs.erect_domain().await?;
        mcs.attach_user().await?;
        mcs.recv_attach_user_confirm().await?;
        #[cfg(debug_assertions)]
        eprintln!("[client] attach_user done: user_channel={}", mcs.user_channel);

        // Step 6: Join all channels
        let user_channel = mcs.user_channel;
        let io_channel = mcs.io_channel;
        mcs.channel_join(user_channel).await?;
        mcs.recv_channel_join_confirm().await?;
        mcs.channel_join(io_channel).await?;
        mcs.recv_channel_join_confirm().await?;
        for &ch in &server_data.channels.clone() {
            mcs.channel_join(ch).await?;
            mcs.recv_channel_join_confirm().await?;
        }

        // Step 7: Send ClientInfo PDU (with 4-byte security header for enhanced security)
        #[cfg(debug_assertions)]
        eprintln!("[client] step7: send ClientInfo");
        let info_pdu = build_client_info(domain, user, password);
        mcs.send_data(io_channel, &info_pdu).await?;

        // Steps 8-9: License exchange loop + Demand Active PDU
        // License PDUs start with [0x80, 0x00] (SEC_LICENSE_PKT in LE).
        // Demand Active has no security header and starts with a ShareControlHeader.
        let mut share_id = 0u32;
        loop {
            let (_, data) = mcs.recv_data().await?;
            if data.is_empty() {
                continue;
            }
            if data[0] == 0x80 && data[1] == 0x00 {
                // License PDU — security header occupies first 4 bytes
                if data.len() >= 4 {
                    let _ = parse_license_pdu(&data[4..]);
                }
            } else {
                // No security header — should be a ShareControlHeader
                let mut pos = 0;
                if let Ok(hdr) = ShareControlHeader::parse(&data, &mut pos) {
                    if hdr.pdu_type == PDUTYPE_DEMANDACTIVEPDU && pos + 4 <= data.len() {
                        share_id = read_u32_le(&data, &mut pos);
                        #[cfg(debug_assertions)]
                        eprintln!("[client] Demand Active received: share_id=0x{:08x}", share_id);
                        break;
                    }
                }
            }
        }

        // Step 10: Confirm Active PDU
        #[cfg(debug_assertions)]
        eprintln!("[client] step10: send Confirm Active");
        let caps = build_all_capabilities(width, height, kbd_layout);
        let mut confirm_body = Vec::new();
        write_u32_le(&mut confirm_body, share_id);
        write_u16_le(&mut confirm_body, 0x03EA); // originatorId
        write_u16_le(&mut confirm_body, 4); // lengthSourceDescriptor
        write_u16_le(&mut confirm_body, (4 + caps.len()) as u16); // lengthCombinedCapabilities (4 = numberCapabilities(2) + pad(2))
        confirm_body.extend_from_slice(b"RDP\0");
        write_u16_le(&mut confirm_body, 11); // numberCapabilities
        write_u16_le(&mut confirm_body, 0); // pad2Octets
        confirm_body.extend_from_slice(&caps);
        let confirm_sch = ShareControlHeader::build(PDUTYPE_CONFIRMACTIVEPDU, user_channel, confirm_body.len());
        let confirm_pdu = [confirm_sch, confirm_body].concat();
        mcs.send_data(io_channel, &confirm_pdu).await?;

        // Step 11: Synchronize sequence
        // Synchronize PDU
        let mut sync_body = Vec::new();
        write_u16_le(&mut sync_body, 1u16); // SYNCMSGTYPE_SYNC
        write_u16_le(&mut sync_body, 0x03EA);
        let sync_pdu = build_data_pdu(share_id, user_channel, PDUTYPE2_SYNCHRONIZE, &sync_body);
        mcs.send_data(io_channel, &sync_pdu).await?;

        // Control Cooperate
        let mut ctrl_coop = Vec::new();
        write_u16_le(&mut ctrl_coop, 14u16); // CTRLACTION_COOPERATE
        write_u16_le(&mut ctrl_coop, 0u16);
        write_u32_le(&mut ctrl_coop, 0u32);
        let ctrl_coop_pdu = build_data_pdu(share_id, user_channel, PDUTYPE2_CONTROL, &ctrl_coop);
        mcs.send_data(io_channel, &ctrl_coop_pdu).await?;

        // Control Request
        let mut ctrl_req = Vec::new();
        write_u16_le(&mut ctrl_req, 4u16); // CTRLACTION_REQUESTCONTROL
        write_u16_le(&mut ctrl_req, 0u16);
        write_u32_le(&mut ctrl_req, 0u32);
        let ctrl_req_pdu = build_data_pdu(share_id, user_channel, PDUTYPE2_CONTROL, &ctrl_req);
        mcs.send_data(io_channel, &ctrl_req_pdu).await?;

        // FontList PDU
        let mut font_body = Vec::new();
        write_u16_le(&mut font_body, 0u16); // numberFonts
        write_u16_le(&mut font_body, 0u16); // totalNumFonts
        write_u16_le(&mut font_body, 0x0003u16); // listFlags
        write_u16_le(&mut font_body, 0x0032u16); // entrySize
        let font_pdu = build_data_pdu(share_id, user_channel, PDUTYPE2_FONTLIST, &font_body);
        mcs.send_data(io_channel, &font_pdu).await?;

        // Step 12: Wait for FontMap (session is ready after this)
        #[cfg(debug_assertions)]
        eprintln!("[client] step12: waiting for FontMap");
        loop {
            let (ch, data) = mcs.recv_data().await?;
            if ch == 0xFFFF {
                continue; // FastPath — ignore during setup
            }
            let mut pos = 0;
            if let Ok(hdr) = ShareControlHeader::parse(&data, &mut pos) {
                if hdr.pdu_type == PDUTYPE_DATAPDU {
                    if let Ok(dh) = ShareDataHeader::parse(&data, &mut pos) {
                        if dh.pdu_type2 == PDUTYPE2_FONTMAP {
                            #[cfg(debug_assertions)]
                            eprintln!("[client] FontMap received: session ready");
                            break;
                        }
                    }
                }
            }
        }

        Ok(RdpSession {
            mcs,
            share_id,
            io_channel,
            user_channel,
            width,
            height,
            kbd_layout,
        })
    }

    /// Receive the next display event from the server.
    pub async fn recv_event(&mut self) -> Result<RdpEvent, RdpError> {
        loop {
            let (ch, data) = self.mcs.recv_data().await?;

            if ch == 0xFFFF {
                // FastPath update
                let bitmaps = parse_fastpath_updates(&data);
                if !bitmaps.is_empty() {
                    return Ok(RdpEvent::Bitmap(bitmaps));
                }
                continue;
            }

            let mut pos = 0;
            let hdr = match ShareControlHeader::parse(&data, &mut pos) {
                Ok(h) => h,
                Err(_) => continue,
            };

            match hdr.pdu_type {
                PDUTYPE_DEACTIVATEALLPDU => {
                    return Ok(RdpEvent::Deactivated);
                }
                PDUTYPE_DATAPDU => {
                    let dh = match ShareDataHeader::parse(&data, &mut pos) {
                        Ok(h) => h,
                        Err(_) => continue,
                    };
                    if dh.pdu_type2 == PDUTYPE2_UPDATE && pos + 2 <= data.len() {
                        let update_type = u16::from_le_bytes([data[pos], data[pos + 1]]);
                        pos += 2;
                        if update_type == UPDATETYPE_BITMAP {
                            let bitmaps = parse_bitmap_update(&data, &mut pos);
                            if !bitmaps.is_empty() {
                                return Ok(RdpEvent::Bitmap(bitmaps));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub async fn send_key_down(&mut self, flags: u16, scancode: u8) -> Result<(), RdpError> {
        let event = build_keyboard_event(flags, scancode);
        let body = wrap_input_pdu(&[event]);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_INPUT, &body);
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    pub async fn send_key_up(&mut self, scancode: u8) -> Result<(), RdpError> {
        let event = build_keyboard_event(KBDFLAGS_KEYUP, scancode);
        let body = wrap_input_pdu(&[event]);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_INPUT, &body);
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    pub async fn send_mouse_move(&mut self, x: u16, y: u16) -> Result<(), RdpError> {
        let event = build_mouse_event(PTRFLAGS_MOVE, x, y);
        let body = wrap_input_pdu(&[event]);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_INPUT, &body);
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    pub async fn send_mouse_button(
        &mut self,
        button: u8,
        down: bool,
        x: u16,
        y: u16,
    ) -> Result<(), RdpError> {
        let btn_flag = match button {
            1 => PTRFLAGS_BUTTON1,
            2 => PTRFLAGS_BUTTON2,
            3 => PTRFLAGS_BUTTON3,
            _ => PTRFLAGS_BUTTON1,
        };
        let flags = if down { btn_flag | PTRFLAGS_DOWN } else { btn_flag };
        let event = build_mouse_event(flags, x, y);
        let body = wrap_input_pdu(&[event]);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_INPUT, &body);
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    pub async fn send_mouse_wheel(&mut self, delta: i16) -> Result<(), RdpError> {
        let (wheel_flags, abs_delta) = if delta < 0 {
            (PTRFLAGS_WHEEL | PTRFLAGS_WHEEL_NEGATIVE, (-delta) as u16)
        } else {
            (PTRFLAGS_WHEEL, delta as u16)
        };
        let flags = wheel_flags | (abs_delta & 0x01FF);
        let event = build_mouse_event(flags, 0, 0);
        let body = wrap_input_pdu(&[event]);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_INPUT, &body);
        self.mcs.send_data(self.io_channel, &pdu).await
    }
}

/// Build a Data PDU: ShareControlHeader + ShareDataHeader + body.
fn build_data_pdu(share_id: u32, user_channel: u16, pdu_type2: u8, body: &[u8]) -> Vec<u8> {
    let sdh = ShareDataHeader::build(share_id, pdu_type2, body.len());
    let inner: Vec<u8> = [sdh, body.to_vec()].concat();
    let sch = ShareControlHeader::build(PDUTYPE_DATAPDU, user_channel, inner.len());
    [sch, inner].concat()
}

/// Build the ClientInfo PDU (includes 4-byte security header and extended info).
fn build_client_info(domain: &str, user: &str, password: &str) -> Vec<u8> {
    let domain_utf16 = to_utf16_le(domain);
    let user_utf16 = to_utf16_le(user);
    let pass_utf16 = to_utf16_le(password);

    // INFO_MOUSE | INFO_DISABLECTRLALTDEL | INFO_AUTOLOGON | INFO_UNICODE | INFO_MAXIMIZESHELL | INFO_ENABLEWINDOWSKEY
    let flags: u32 = 0x0001 | 0x0002 | 0x0008 | 0x0010 | 0x0020 | 0x0100;

    let mut info = Vec::new();

    // 4-byte security header (SEC_INFO_PKT = 0x0040)
    write_u16_le(&mut info, 0x0040);
    write_u16_le(&mut info, 0x0000);

    // TS_INFO_PACKET
    write_u32_le(&mut info, 0); // CodePage
    write_u32_le(&mut info, flags);
    write_u16_le(&mut info, domain_utf16.len() as u16); // cbDomain (bytes, without null)
    write_u16_le(&mut info, user_utf16.len() as u16); // cbUserName
    write_u16_le(&mut info, pass_utf16.len() as u16); // cbPassword
    write_u16_le(&mut info, 0); // cbAlternateShell
    write_u16_le(&mut info, 0); // cbWorkingDir
    // Fields include null terminators
    info.extend_from_slice(&domain_utf16);
    info.extend_from_slice(&[0, 0]);
    info.extend_from_slice(&user_utf16);
    info.extend_from_slice(&[0, 0]);
    info.extend_from_slice(&pass_utf16);
    info.extend_from_slice(&[0, 0]);
    info.extend_from_slice(&[0, 0]); // AlternateShell
    info.extend_from_slice(&[0, 0]); // WorkingDir

    // TS_EXTENDED_INFO_PACKET
    write_u16_le(&mut info, 2); // clientAddressFamily (AF_INET)
    let addr_utf16 = to_utf16_le("127.0.0.1");
    write_u16_le(&mut info, (addr_utf16.len() + 2) as u16); // cbClientAddress (with null)
    info.extend_from_slice(&addr_utf16);
    info.extend_from_slice(&[0, 0]);
    write_u16_le(&mut info, 2); // cbClientDir (empty string, just null)
    info.extend_from_slice(&[0, 0]);
    info.extend_from_slice(&[0u8; 172]); // clientTimeZone
    write_u32_le(&mut info, 0); // clientSessionId
    // performanceFlags: disable wallpaper, themes, cursor shadow
    write_u32_le(&mut info, 0x0020 | 0x0080 | 0x0400);
    write_u16_le(&mut info, 0); // cbAutoReconnectCookie

    info
}

fn parse_bitmap_update(data: &[u8], pos: &mut usize) -> Vec<Bitmap> {
    if *pos + 2 > data.len() {
        return vec![];
    }
    let n_rects = read_u16_le(data, pos) as usize;
    let mut bitmaps = Vec::with_capacity(n_rects);

    for _ in 0..n_rects {
        if *pos + 18 > data.len() {
            break;
        }
        let dest_left = read_u16_le(data, pos) as i32;
        let dest_top = read_u16_le(data, pos) as i32;
        let dest_right = read_u16_le(data, pos) as i32;
        let dest_bottom = read_u16_le(data, pos) as i32;
        let width = read_u16_le(data, pos) as i32;
        let height = read_u16_le(data, pos) as i32;
        let bpp = read_u16_le(data, pos) as i32;
        let flags = read_u16_le(data, pos);
        let bitmap_len = read_u16_le(data, pos) as usize;

        if *pos + bitmap_len > data.len() {
            break;
        }
        let raw = &data[*pos..*pos + bitmap_len];
        *pos += bitmap_len;

        let pixel_data = if flags & BITMAP_COMPRESSION != 0 {
            let compressed = if flags & NO_BITMAP_COMPRESSION_HDR == 0 && raw.len() > 8 {
                &raw[8..] // skip 8-byte compression header
            } else {
                raw
            };
            rle::decompress(compressed, width as usize, height as usize, bpp as usize)
        } else {
            // Uncompressed bitmaps are stored bottom-up; flip for display
            flip_vertical(raw, width as usize, height as usize, bpp as usize)
        };

        bitmaps.push(Bitmap {
            dest_left,
            dest_top,
            dest_right,
            dest_bottom,
            width,
            height,
            bits_per_pixel: bpp,
            data: pixel_data,
        });
    }

    bitmaps
}

fn flip_vertical(data: &[u8], width: usize, height: usize, bpp: usize) -> Vec<u8> {
    let bytes_per_pixel = (bpp + 7) / 8;
    let stride = width * bytes_per_pixel;
    let total = stride * height;
    let mut out = vec![0u8; total];
    for row in 0..height {
        let src_row = height - 1 - row;
        let src_start = src_row * stride;
        let dst_start = row * stride;
        if src_start + stride <= data.len() && dst_start + stride <= out.len() {
            out[dst_start..dst_start + stride]
                .copy_from_slice(&data[src_start..src_start + stride]);
        }
    }
    out
}

fn parse_fastpath_updates(data: &[u8]) -> Vec<Bitmap> {
    let mut bitmaps = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let header = data[pos];
        pos += 1;
        let update_code = header & 0x0F;
        let compression = (header >> 6) & 0x03;

        if compression != 0 {
            if pos >= data.len() {
                break;
            }
            pos += 1; // compressionFlags byte
        }

        if pos + 2 > data.len() {
            break;
        }
        let size = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + size > data.len() {
            break;
        }
        let update_data = &data[pos..pos + size];
        pos += size;

        if update_code == 0x01 {
            // FASTPATH_UPDATETYPE_BITMAP
            let mut p = 0;
            let mut rects = parse_bitmap_update(update_data, &mut p);
            bitmaps.append(&mut rects);
        }
    }

    bitmaps
}
