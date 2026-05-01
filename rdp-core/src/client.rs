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
    PDUTYPE2_FONTMAP, PDUTYPE2_FONTLIST, PDUTYPE2_SUPPRESS_OUTPUT,
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
    /// Buffer for reassembling multi-PDU fragmented FastPath updates
    frag_buf: Vec<u8>,
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
                log::debug!("[client] step1: X.224 connect");
        x224.connect(user).await?;
        let selected_protocol = x224.recv_confirm().await?;
                log::debug!("[client] step1 done: selected_protocol=0x{:08x}", selected_protocol);

        // Step 2: TLS upgrade (proxy performs TLS and sends back the server public key)
        let pub_key = if selected_protocol == PROTOCOL_SSL || selected_protocol == PROTOCOL_HYBRID {
                        log::debug!("[client] step2: start_tls");
            let pk = x224.tpkt_mut().transport_mut().start_tls().await?;
                        log::debug!("[client] step2 done: pub_key len={}", pk.len());
            pk
        } else {
            Vec::new()
        };

        // Step 3: CredSSP / NLA authentication
        let transport = if selected_protocol == PROTOCOL_HYBRID {
                        log::debug!("[client] step3: CredSSP/NLA authenticate");
            let inner_transport = x224.into_tpkt().into_transport();
            let mut cssp = Cssp::new(inner_transport, domain, user, password);
            cssp.authenticate(&pub_key).await?;
                        log::debug!("[client] step3 done: CredSSP complete");
            cssp.into_transport()
        } else {
            x224.into_tpkt().into_transport()
        };

        // Step 4: Re-wrap transport (after CredSSP the TLS stream is reused)
        let tpkt = Tpkt::new(transport);
        let mut x224 = X224::new(tpkt);
        x224.selected_protocol = selected_protocol;

        // Step 5: MCS / GCC connection
                log::debug!("[client] step5: MCS connect");
        let client_data = ClientData {
            width,
            height,
            kbd_layout,
            color_depth: 0xca01,
            server_selected_protocol: selected_protocol,
            ..ClientData::default()
        };
        let gcc_data = create_gcc_data(&client_data);
        let mut mcs = McsClient::new(x224);
        mcs.connect(&gcc_data).await?;
        let server_data = mcs.recv_connect_response().await?;
        mcs.io_channel = server_data.io_channel;
                log::debug!("[client] step5 done: io_channel={} channels={:?}", mcs.io_channel, server_data.channels);
        mcs.erect_domain().await?;
        mcs.attach_user().await?;
        mcs.recv_attach_user_confirm().await?;
                log::debug!("[client] attach_user done: user_channel={}", mcs.user_channel);

        // Step 6: Join all channels
        let user_channel = mcs.user_channel;
        let io_channel = mcs.io_channel;
        mcs.channel_join(user_channel).await?;
        mcs.recv_channel_join_confirm().await?;
        mcs.channel_join(io_channel).await?;
        mcs.recv_channel_join_confirm().await?;
        if let Some(msg_ch) = server_data.msg_channel {
                        log::debug!("[client] joining msg_channel={}", msg_ch);
            mcs.channel_join(msg_ch).await?;
            mcs.recv_channel_join_confirm().await?;
        }
        for &ch in &server_data.channels.clone() {
            mcs.channel_join(ch).await?;
            mcs.recv_channel_join_confirm().await?;
        }

        // Step 7: Send ClientInfo PDU (with 4-byte security header for enhanced security)
                log::debug!("[client] step7: send ClientInfo");
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
                                                log::debug!("[client] Demand Active received: share_id=0x{:08x}", share_id);
                        break;
                    }
                }
            }
        }

        let mut session = RdpSession {
            mcs,
            share_id,
            io_channel,
            user_channel,
            width,
            height,
            kbd_layout,
            frag_buf: Vec::new(),
        };

        // Steps 10-12: Confirm Active, sync sequence, wait for FontMap
        session.complete_activation().await?;

        // Tell the server to start sending display updates (MS-RDPBCGR 2.2.11.3.1)
        session.send_suppress_output().await?;

        Ok(session)
    }

    /// Confirm Active PDU + synchronize sequence + wait for FontMap.
    /// Called after login and after each Deactivate/Reactivate cycle.
    async fn complete_activation(&mut self) -> Result<(), RdpError> {
        log::debug!("[client] complete_activation: share_id=0x{:08x}", self.share_id);

        // Confirm Active PDU
        let (caps, num_caps) = build_all_capabilities(self.width, self.height, self.kbd_layout);
        let mut confirm_body = Vec::new();
        write_u32_le(&mut confirm_body, self.share_id);
        write_u16_le(&mut confirm_body, 0x03EA); // originatorId
        write_u16_le(&mut confirm_body, 4); // lengthSourceDescriptor
        write_u16_le(&mut confirm_body, (4 + caps.len()) as u16); // lengthCombinedCapabilities
        confirm_body.extend_from_slice(b"RDP\0");
        write_u16_le(&mut confirm_body, num_caps);
        write_u16_le(&mut confirm_body, 0); // pad2Octets
        confirm_body.extend_from_slice(&caps);
        let confirm_sch = ShareControlHeader::build(
            PDUTYPE_CONFIRMACTIVEPDU, self.user_channel, confirm_body.len(),
        );
        let confirm_pdu = [confirm_sch, confirm_body].concat();
        self.mcs.send_data(self.io_channel, &confirm_pdu).await?;

        // Synchronize PDU
        let mut sync_body = Vec::new();
        write_u16_le(&mut sync_body, 1u16); // SYNCMSGTYPE_SYNC
        write_u16_le(&mut sync_body, 0x03EA);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_SYNCHRONIZE, &sync_body);
        self.mcs.send_data(self.io_channel, &pdu).await?;

        // Control Cooperate
        let mut ctrl = Vec::new();
        write_u16_le(&mut ctrl, 4u16); // CTRLACTION_COOPERATE
        write_u16_le(&mut ctrl, 0u16);
        write_u32_le(&mut ctrl, 0u32);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_CONTROL, &ctrl);
        self.mcs.send_data(self.io_channel, &pdu).await?;

        // Control Request
        let mut ctrl = Vec::new();
        write_u16_le(&mut ctrl, 1u16); // CTRLACTION_REQUESTCONTROL
        write_u16_le(&mut ctrl, 0u16);
        write_u32_le(&mut ctrl, 0u32);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_CONTROL, &ctrl);
        self.mcs.send_data(self.io_channel, &pdu).await?;

        // FontList PDU
        let mut font = Vec::new();
        write_u16_le(&mut font, 0u16); // numberFonts
        write_u16_le(&mut font, 0u16); // totalNumFonts
        write_u16_le(&mut font, 0x0003u16); // listFlags
        write_u16_le(&mut font, 0x0032u16); // entrySize
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_FONTLIST, &font);
        self.mcs.send_data(self.io_channel, &pdu).await?;

        // Wait for FontMap
        log::debug!("[client] complete_activation: waiting for FontMap");
        loop {
            let (ch, data) = self.mcs.recv_data().await?;
            if ch == 0xFFFF {
                continue;
            }
            let mut pos = 0;
            if let Ok(hdr) = ShareControlHeader::parse(&data, &mut pos) {
                log::debug!("[client] complete_activation recv: pdu_type=0x{:04x}", hdr.pdu_type);
                if hdr.pdu_type == PDUTYPE_DATAPDU {
                    if let Ok(dh) = ShareDataHeader::parse(&data, &mut pos) {
                        if dh.pdu_type2 == PDUTYPE2_FONTMAP {
                            log::debug!("[client] FontMap received: session ready");
                            break;
                        }
                        if dh.pdu_type2 == 0x2F && pos + 4 <= data.len() {
                            let err_code = u32::from_le_bytes([
                                data[pos], data[pos+1], data[pos+2], data[pos+3],
                            ]);
                            log::error!("[client] ERROR INFO PDU: error_code=0x{:08x}", err_code);
                        }
                    }
                } else if hdr.pdu_type == PDUTYPE_DEACTIVATEALLPDU {
                    log::warn!("[client] DEACTIVATE_ALL received during activation, ignoring");
                }
            }
        }

        Ok(())
    }

    /// Tell the server to resume sending display updates.
    async fn send_suppress_output(&mut self) -> Result<(), RdpError> {
        let mut body = Vec::with_capacity(12);
        body.push(1u8); // ALLOW_DISPLAY_UPDATES
        body.extend_from_slice(&[0u8; 3]);
        write_u16_le(&mut body, 0);
        write_u16_le(&mut body, 0);
        write_u16_le(&mut body, self.width - 1);
        write_u16_le(&mut body, self.height - 1);
        let pdu = build_data_pdu(self.share_id, self.user_channel, PDUTYPE2_SUPPRESS_OUTPUT, &body);
        log::debug!("[client] sending SuppressOutput (ALLOW_DISPLAY_UPDATES)");
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    /// Receive the next display event from the server.
    pub async fn recv_event(&mut self) -> Result<RdpEvent, RdpError> {
        log::debug!("[recv_event] entering");
        loop {
            log::debug!("[recv_event] waiting for recv_data...");
            let result = self.mcs.recv_data().await;
            log::debug!("[recv_event] recv_data returned: {}", if result.is_ok() { "Ok" } else { "Err" });
            let (ch, data) = result?;

            if ch == 0xFFFF {
                // FastPath update
                log::debug!("[recv_event] FastPath data_len={}", data.len());
                let bitmaps = parse_fastpath_updates(&data, &mut self.frag_buf);
                log::debug!("[recv_event] FastPath bitmaps={}", bitmaps.len());
                if !bitmaps.is_empty() {
                    return Ok(RdpEvent::Bitmap(bitmaps));
                }
                continue;
            }

            log::debug!("[recv_event] ch={} data_len={}", ch, data.len());

            let mut pos = 0;
            let hdr = match ShareControlHeader::parse(&data, &mut pos) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("[recv_event] ShareControlHeader parse error: {:?}", e);
                    continue;
                }
            };

            log::debug!("[recv_event] pdu_type=0x{:04x}", hdr.pdu_type);

            match hdr.pdu_type {
                PDUTYPE_DEACTIVATEALLPDU => {
                    log::info!("[recv_event] DeactivateAll — waiting for new Demand Active");
                    // Re-run the activation sequence instead of dropping the connection.
                    // The server will immediately follow with a new Demand Active.
                    loop {
                        let (ch2, data2) = self.mcs.recv_data().await?;
                        if ch2 == 0xFFFF { continue; }
                        let mut pos2 = 0;
                        if let Ok(hdr2) = ShareControlHeader::parse(&data2, &mut pos2) {
                            if hdr2.pdu_type == PDUTYPE_DEMANDACTIVEPDU && pos2 + 4 <= data2.len() {
                                self.share_id = read_u32_le(&data2, &mut pos2);
                                log::info!("[recv_event] new Demand Active: share_id=0x{:08x}", self.share_id);
                                break;
                            }
                        }
                    }
                    self.complete_activation().await?;
                    self.send_suppress_output().await?;
                    // Continue receiving normal display events
                }
                PDUTYPE_DATAPDU => {
                    let dh = match ShareDataHeader::parse(&data, &mut pos) {
                        Ok(h) => h,
                        Err(e) => {
                            log::warn!("[recv_event] ShareDataHeader parse error: {:?}", e);
                            continue;
                        }
                    };
                    log::debug!("[recv_event] pduType2={}", dh.pdu_type2);
                    if dh.pdu_type2 == 0x2F && pos + 4 <= data.len() {
                        let err_code = u32::from_le_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]);
                        log::error!("[recv_event] ERROR INFO PDU: error_code=0x{:08x}", err_code);
                    }
                    if dh.pdu_type2 == PDUTYPE2_UPDATE && pos + 2 <= data.len() {
                        let update_type = u16::from_le_bytes([data[pos], data[pos + 1]]);
                        pos += 2;
                        log::debug!("[recv_event] update_type=0x{:04x}", update_type);
                        if update_type == UPDATETYPE_BITMAP {
                            let bitmaps = parse_bitmap_update(&data, &mut pos);
                            log::debug!("[recv_event] slow-path bitmaps={}", bitmaps.len());
                            if !bitmaps.is_empty() {
                                return Ok(RdpEvent::Bitmap(bitmaps));
                            }
                        }
                    }
                }
                _ => {
                    log::debug!("[recv_event] unhandled pdu_type=0x{:04x}", hdr.pdu_type);
                }
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

    // Match grdp flags: INFO_MOUSE | INFO_DISABLECTRLALTDEL | INFO_AUTOLOGON | INFO_UNICODE |
    // INFO_MAXIMIZESHELL | INFO_ENABLEWINDOWSKEY | INFO_NOAUDIOPLAYBACK | INFO_VIDEO_DISABLE
    let flags: u32 = 0x0001 | 0x0002 | 0x0008 | 0x0010 | 0x0020 | 0x0100 | 0x4000 | 0x00020000;

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
    write_u16_le(&mut info, 2); // cbClientAddress: empty string (just null terminator)
    info.extend_from_slice(&[0, 0]); // clientAddress: null
    write_u16_le(&mut info, 2); // cbClientDir (empty string, just null)
    info.extend_from_slice(&[0, 0]);
    info.extend_from_slice(&[0u8; 172]); // clientTimeZone
    write_u32_le(&mut info, 0); // clientSessionId
    // performanceFlags: match grdp 0x00000187
    // PERF_DISABLE_WALLPAPER(0x01) | PERF_DISABLE_FULLWINDOWDRAG(0x02) |
    // PERF_DISABLE_MENUANIMATIONS(0x04) | PERF_DISABLE_THEMING(0x80) |
    // PERF_DISABLE_CURSOR_SHADOW(0x100)
    write_u32_le(&mut info, 0x00000187);

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
            log::warn!("[bitmap] bitmap_len={} exceeds remaining data, breaking", bitmap_len);
            break;
        }
        let raw = &data[*pos..*pos + bitmap_len];
        *pos += bitmap_len;

        let pixel_data = if flags & BITMAP_COMPRESSION != 0 {
            let compressed = if flags & NO_BITMAP_COMPRESSION_HDR == 0 && raw.len() >= 8 {
                // Parse TS_CD_HEADER: cbCompFirstRowSize(2) + cbCompMainBodySize(2) +
                //                     cbScanWidth(2) + cbUncompressedSize(2)
                let cb_main = u16::from_le_bytes([raw[2], raw[3]]) as usize;
                let end = 8 + cb_main.min(raw.len() - 8);
                &raw[8..end]
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
    let stride = width.saturating_mul(bytes_per_pixel);
    let total = stride.saturating_mul(height);
    if total > 32 * 1024 * 1024 {
        log::warn!("flip_vertical: suspiciously large total={}, skipping", total);
        return vec![];
    }
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

fn parse_fastpath_updates(data: &[u8], frag_buf: &mut Vec<u8>) -> Vec<Bitmap> {
    let mut bitmaps = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let header = data[pos];
        pos += 1;
        let update_code = header & 0x0F;
        let fragmentation = (header >> 4) & 0x03;
        let compression = (header >> 6) & 0x03;

        // compressionFlags byte is present only when FASTPATH_OUTPUT_COMPRESSION_USED (0x2)
        if compression == 0x02 {
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
            log::warn!("[fastpath] update_code={} frag={} size={} exceeds remaining={}", update_code, fragmentation, size, data.len() - pos);
            break;
        }
        let update_data = &data[pos..pos + size];
        pos += size;

        log::debug!("[fastpath] update_code=0x{:02x} frag={} size={}", update_code, fragmentation, size);

        if update_code != 0x01 {
            continue;
        }

        // Fragment reassembly (MS-RDPBCGR 2.2.9.1.2.1.2)
        // frag: 0x00=SINGLE, 0x01=LAST, 0x02=FIRST, 0x03=NEXT
        match fragmentation {
            0x00 => {
                // Non-fragmented: skip 2-byte updateType header, then parse rects
                let mut p = 2;
                let mut rects = parse_bitmap_update(update_data, &mut p);
                bitmaps.append(&mut rects);
            }
            0x02 => {
                // FIRST fragment: start accumulating
                frag_buf.clear();
                frag_buf.extend_from_slice(update_data);
            }
            0x03 => {
                // NEXT fragment: append
                frag_buf.extend_from_slice(update_data);
            }
            0x01 => {
                // LAST fragment: append and process reassembled data
                // Reassembled buffer starts with 2-byte updateType header (from FIRST)
                frag_buf.extend_from_slice(update_data);
                let reassembled = frag_buf.clone();
                frag_buf.clear();
                let mut p = 2;
                let mut rects = parse_bitmap_update(&reassembled, &mut p);
                bitmaps.append(&mut rects);
            }
            _ => {}
        }
    }

    bitmaps
}
