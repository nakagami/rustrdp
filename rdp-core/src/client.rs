use crate::bitmap::Bitmap;
use crate::core::io::*;
use crate::core::rle;
use crate::error::RdpError;
use crate::protocol::drdynvc::DrdynvcHandler;
use crate::protocol::lic::parse_license_pdu;
use crate::protocol::nla::cssp::Cssp;
use crate::protocol::nla::ntlm::to_utf16_le;
use crate::protocol::pdu::caps::build_all_capabilities;
use crate::protocol::pdu::input::{
    build_keyboard_event, build_mouse_event, wrap_input_pdu, KBDFLAGS_KEYUP, PTRFLAGS_BUTTON1,
    PTRFLAGS_BUTTON2, PTRFLAGS_BUTTON3, PTRFLAGS_DOWN, PTRFLAGS_MOVE, PTRFLAGS_WHEEL,
    PTRFLAGS_WHEEL_NEGATIVE,
};
use crate::protocol::pdu::{
    ShareControlHeader, ShareDataHeader, PDUTYPE2_CONTROL, PDUTYPE2_FONTLIST, PDUTYPE2_FONTMAP,
    PDUTYPE2_INPUT, PDUTYPE2_SUPPRESS_OUTPUT, PDUTYPE2_SYNCHRONIZE, PDUTYPE2_UPDATE,
    PDUTYPE_CONFIRMACTIVEPDU, PDUTYPE_DATAPDU, PDUTYPE_DEACTIVATEALLPDU, PDUTYPE_DEMANDACTIVEPDU,
    UPDATETYPE_BITMAP,
};
use crate::protocol::rdpsnd::{AudioFormat, RdpsndHandler};
use crate::protocol::t125::gcc::{create_gcc_data, ClientData};
use crate::protocol::t125::mcs::McsClient;
use crate::protocol::tpkt::Tpkt;
use crate::protocol::x224::{PROTOCOL_HYBRID, PROTOCOL_SSL, X224};
use crate::protocol::Transport;

const BITMAP_COMPRESSION: u16 = 0x0001;
const NO_BITMAP_COMPRESSION_HDR: u16 = 0x0400;
const BITMAP_NO_PROCESSING: u16 = 0x8000;
const PDUTYPE2_FRAME_ACKNOWLEDGE: u8 = 0x38;
const FASTPATH_UPDATETYPE_BITMAP: u8 = 0x01;
const FASTPATH_UPDATETYPE_SURFCMDS: u8 = 0x04;
const CMDTYPE_SET_SURFACE_BITS: u16 = 0x0001;
const CMDTYPE_FRAME_MARKER: u16 = 0x0004;
const CMDTYPE_STREAM_SURFACE_BITS: u16 = 0x0006;
const SURFCMD_FRAMEACTION_END: u16 = 0x0001;

pub enum RdpEvent {
    Ready,
    Bitmap(Vec<Bitmap>),
    Resize { width: u16, height: u16 },
    Deactivated,
    Audio { format: AudioFormat, data: Vec<u8> },
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
    /// Channel ID assigned to the "rdpsnd" static virtual channel
    rdpsnd_channel: Option<u16>,
    /// RDPSND protocol state machine
    rdpsnd_handler: RdpsndHandler,
    /// Fragment reassembly buffer for rdpsnd channel PDUs
    rdpsnd_frag: Vec<u8>,
    /// Total byte length of the current rdpsnd fragment chain
    rdpsnd_frag_total: usize,
    /// Channel ID assigned to the "drdynvc" static virtual channel
    drdynvc_channel: Option<u16>,
    /// DRDYNVC protocol handler
    drdynvc_handler: DrdynvcHandler,
    /// Fragment reassembly buffer for drdynvc channel PDUs
    drdynvc_frag: Vec<u8>,
    /// Total byte length of the current drdynvc fragment chain
    drdynvc_frag_total: usize,
    /// Pending audio events from DVC that haven't been delivered yet
    pending_audio: std::collections::VecDeque<crate::protocol::rdpsnd::AudioEvent>,
    /// Timestamp of the last force-refresh (suppress→allow) sent to the server.
    /// Used to rate-limit keyframe requests to at most once every 2 seconds.
    last_force_refresh: Option<std::time::Instant>,
    /// Frame IDs that still need acknowledgment. When recv_event() is cancelled by an
    /// external timeout (e.g. tokio::time::timeout) between recv_data() returning and
    /// send_frame_acknowledge() completing, these IDs are preserved here so the next
    /// recv_event() call sends them before waiting for new data. Without this, unacknowledged
    /// frames cause the server to stop sending display updates (RDPGFX backpressure).
    pending_acks: std::collections::VecDeque<u32>,
}

impl<T: Transport> RdpSession<T> {
    /// Perform full RDP login and return a ready session.
    ///
    /// Pass `avc` to enable H.264/AVC video decode.  Use `None` for
    /// environments that do not need hardware video decode (e.g. WASM, CLI).
    pub async fn login(
        transport: T,
        domain: &str,
        user: &str,
        password: &str,
        width: u16,
        height: u16,
        kbd_layout: u32,
        avc: Option<Box<dyn crate::avc::AvcDecoder>>,
    ) -> Result<Self, RdpError> {
        // Step 1: X.224 connection negotiation
        let tpkt = Tpkt::new(transport);
        let mut x224 = X224::new(tpkt);
        log::debug!("[client] step1: X.224 connect");
        x224.connect(user).await?;
        let selected_protocol = x224.recv_confirm().await?;
        log::debug!(
            "[client] step1 done: selected_protocol=0x{:08x}",
            selected_protocol
        );

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
        log::debug!(
            "[client] step5 done: io_channel={} channels={:?}",
            mcs.io_channel,
            server_data.channels
        );
        mcs.erect_domain().await?;
        mcs.attach_user().await?;
        mcs.recv_attach_user_confirm().await?;
        log::debug!(
            "[client] attach_user done: user_channel={}",
            mcs.user_channel
        );

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
                        log::debug!(
                            "[client] Demand Active received: share_id=0x{:08x}",
                            share_id
                        );
                        break;
                    }
                }
            }
        }

        // Determine the rdpsnd channel ID.
        // ClientData::default() lists channels as [rdpdr, rdpsnd, drdynvc, cliprdr], so rdpsnd
        // is at index 1 and drdynvc is at index 2 in the server's channel ID array.
        let rdpsnd_channel = server_data.channels.get(1).copied();
        log::debug!("[client] rdpsnd_channel={:?}", rdpsnd_channel);
        let drdynvc_channel = server_data.channels.get(2).copied();
        log::debug!("[client] drdynvc_channel={:?}", drdynvc_channel);

        let mut session = RdpSession {
            mcs,
            share_id,
            io_channel,
            user_channel,
            width,
            height,
            kbd_layout,
            frag_buf: Vec::new(),
            rdpsnd_channel,
            rdpsnd_handler: RdpsndHandler::new(),
            rdpsnd_frag: Vec::new(),
            rdpsnd_frag_total: 0,
            drdynvc_channel,
            drdynvc_handler: {
                let mut h = DrdynvcHandler::new();
                h.set_avc_decoder(avc);
                h
            },
            drdynvc_frag: Vec::new(),
            drdynvc_frag_total: 0,
            pending_audio: std::collections::VecDeque::new(),
            last_force_refresh: None,
            pending_acks: std::collections::VecDeque::new(),
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
        log::debug!(
            "[client] complete_activation: share_id=0x{:08x}",
            self.share_id
        );

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
            PDUTYPE_CONFIRMACTIVEPDU,
            self.user_channel,
            confirm_body.len(),
        );
        let confirm_pdu = [confirm_sch, confirm_body].concat();
        self.mcs.send_data(self.io_channel, &confirm_pdu).await?;

        // Synchronize PDU
        let mut sync_body = Vec::new();
        write_u16_le(&mut sync_body, 1u16); // SYNCMSGTYPE_SYNC
        write_u16_le(&mut sync_body, 0x03EA);
        let pdu = build_data_pdu(
            self.share_id,
            self.user_channel,
            PDUTYPE2_SYNCHRONIZE,
            &sync_body,
        );
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
                log::debug!(
                    "[client] complete_activation recv: pdu_type=0x{:04x}",
                    hdr.pdu_type
                );
                if hdr.pdu_type == PDUTYPE_DATAPDU {
                    if let Ok(dh) = ShareDataHeader::parse(&data, &mut pos) {
                        if dh.pdu_type2 == PDUTYPE2_FONTMAP {
                            log::debug!("[client] FontMap received: session ready");
                            break;
                        }
                        if dh.pdu_type2 == 0x2F && pos + 4 <= data.len() {
                            let err_code = u32::from_le_bytes([
                                data[pos],
                                data[pos + 1],
                                data[pos + 2],
                                data[pos + 3],
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
        let pdu = build_data_pdu(
            self.share_id,
            self.user_channel,
            PDUTYPE2_SUPPRESS_OUTPUT,
            &body,
        );
        log::debug!("[client] sending SuppressOutput (ALLOW_DISPLAY_UPDATES)");
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    /// Send a suppress→allow SuppressOutput PDU pair to request a fresh IDR keyframe.
    /// The suppress PDU (0x00) has no desktop rectangle (4 bytes total).
    /// The allow PDU (0x01) includes the full desktop rectangle (12 bytes total).
    async fn send_force_refresh(&mut self) -> Result<(), RdpError> {
        // SUPPRESS: AllowDisplayUpdates=0x00 + 3 bytes padding (no rect)
        let suppress_body = [0x00u8, 0x00, 0x00, 0x00];
        let pdu = build_data_pdu(
            self.share_id,
            self.user_channel,
            PDUTYPE2_SUPPRESS_OUTPUT,
            &suppress_body,
        );
        self.mcs.send_data(self.io_channel, &pdu).await?;

        // ALLOW: AllowDisplayUpdates=0x01 + 3 bytes padding + desktop rect
        let mut allow_body = Vec::with_capacity(12);
        allow_body.push(0x01u8); // ALLOW_DISPLAY_UPDATES
        allow_body.extend_from_slice(&[0u8; 3]);
        write_u16_le(&mut allow_body, 0);
        write_u16_le(&mut allow_body, 0);
        write_u16_le(&mut allow_body, self.width - 1);
        write_u16_le(&mut allow_body, self.height - 1);
        let pdu = build_data_pdu(
            self.share_id,
            self.user_channel,
            PDUTYPE2_SUPPRESS_OUTPUT,
            &allow_body,
        );
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    async fn send_frame_acknowledge(&mut self, frame_id: u32) -> Result<(), RdpError> {
        let mut body = Vec::with_capacity(4);
        write_u32_le(&mut body, frame_id);
        let pdu = build_data_pdu(
            self.share_id,
            self.user_channel,
            PDUTYPE2_FRAME_ACKNOWLEDGE,
            &body,
        );
        log::debug!("[client] FastPath surface frame ack frame_id={}", frame_id);
        self.mcs.send_data(self.io_channel, &pdu).await
    }

    /// Receive the next display event from the server.
    pub async fn recv_event(&mut self) -> Result<RdpEvent, RdpError> {
        log::debug!("[recv_event] entering");

        // Flush any frame ACKs that were not sent because the previous recv_event()
        // call was cancelled by an external timeout (e.g. tokio::time::timeout) between
        // recv_data() returning and send_frame_acknowledge() completing.
        // Without this, the server stops sending display updates after a few missed ACKs.
        while let Some(frame_id) = self.pending_acks.pop_front() {
            log::debug!(
                "[recv_event] sending deferred frame ack frame_id={}",
                frame_id
            );
            self.send_frame_acknowledge(frame_id).await?;
        }

        loop {
            // Deliver any pending audio events before waiting for more data
            if let Some(ev) = self.pending_audio.pop_front() {
                return Ok(RdpEvent::Audio {
                    format: ev.format,
                    data: ev.data,
                });
            }

            log::debug!("[recv_event] waiting for recv_data...");
            let result = self.mcs.recv_data().await;
            log::debug!(
                "[recv_event] recv_data returned: {}",
                if result.is_ok() { "Ok" } else { "Err" }
            );
            let (ch, data) = result?;

            if ch == 0xFFFF {
                // FastPath update
                log::debug!("[recv_event] FastPath data_len={}", data.len());
                let result = parse_fastpath_updates(&data, &mut self.frag_buf);
                // Queue ACKs first so they survive cancellation, then send them.
                // If cancelled mid-send, the remaining IDs stay in pending_acks and
                // will be sent at the start of the next recv_event() call.
                self.pending_acks.extend(result.frame_ids.iter().copied());
                while let Some(frame_id) = self.pending_acks.pop_front() {
                    self.send_frame_acknowledge(frame_id).await?;
                }
                let bitmaps = result.bitmaps;
                log::debug!("[recv_event] FastPath bitmaps={}", bitmaps.len());
                if !bitmaps.is_empty() {
                    if bitmaps_contain_large_refresh(&bitmaps) {
                        self.drdynvc_handler.signal_screen_refreshed();
                    }
                    return Ok(RdpEvent::Bitmap(bitmaps));
                }
                continue;
            }

            // Dispatch rdpsnd static virtual channel data
            if Some(ch) == self.rdpsnd_channel {
                if let Some(event) = self.handle_rdpsnd_data(&data).await {
                    return Ok(RdpEvent::Audio {
                        format: event.format,
                        data: event.data,
                    });
                }
                continue;
            }

            // Dispatch drdynvc static virtual channel data
            if Some(ch) == self.drdynvc_channel {
                let (bitmaps, audio_events, reset_size) = self.handle_drdynvc_data(&data).await;
                for ev in audio_events {
                    self.pending_audio.push_back(ev);
                }
                if let Some((width, height)) = reset_size {
                    self.width = width;
                    self.height = height;
                    return Ok(RdpEvent::Resize { width, height });
                }
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
                        if ch2 == 0xFFFF {
                            continue;
                        }
                        let mut pos2 = 0;
                        if let Ok(hdr2) = ShareControlHeader::parse(&data2, &mut pos2) {
                            if hdr2.pdu_type == PDUTYPE_DEMANDACTIVEPDU && pos2 + 4 <= data2.len() {
                                self.share_id = read_u32_le(&data2, &mut pos2);
                                log::info!(
                                    "[recv_event] new Demand Active: share_id=0x{:08x}",
                                    self.share_id
                                );
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
                        let err_code = u32::from_le_bytes([
                            data[pos],
                            data[pos + 1],
                            data[pos + 2],
                            data[pos + 3],
                        ]);
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
                                if bitmaps_contain_large_refresh(&bitmaps) {
                                    self.drdynvc_handler.signal_screen_refreshed();
                                }
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

    /// Handle data arriving on the rdpsnd virtual channel.
    /// Reassembles fragmented channel PDUs (MS-RDPBCGR virtual channel fragmentation)
    /// then passes the complete payload to the RDPSND state machine.
    async fn handle_rdpsnd_data(
        &mut self,
        data: &[u8],
    ) -> Option<crate::protocol::rdpsnd::AudioEvent> {
        // Virtual channel PDU header: length(4) + flags(4)
        const CHANNEL_FLAG_FIRST: u32 = 0x01;
        const CHANNEL_FLAG_LAST: u32 = 0x02;

        if data.len() < 8 {
            log::warn!("[rdpsnd] channel data too short: {} bytes", data.len());
            return None;
        }
        let total_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let flags = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let payload = &data[8..];

        if flags & CHANNEL_FLAG_FIRST != 0 {
            self.rdpsnd_frag.clear();
            self.rdpsnd_frag_total = total_len;
        }
        self.rdpsnd_frag.extend_from_slice(payload);

        if flags & CHANNEL_FLAG_LAST == 0 {
            // More fragments to come
            return None;
        }

        // Complete PDU assembled
        let assembled = std::mem::take(&mut self.rdpsnd_frag);
        let (response, event) = self.rdpsnd_handler.process_data(&assembled);

        if !response.is_empty() {
            // Wrap response in a virtual channel PDU header and send
            let mut vchan_pdu = Vec::with_capacity(8 + response.len());
            vchan_pdu.extend_from_slice(&(response.len() as u32).to_le_bytes()); // length
            let send_flags: u32 = CHANNEL_FLAG_FIRST | CHANNEL_FLAG_LAST;
            vchan_pdu.extend_from_slice(&send_flags.to_le_bytes()); // flags
            vchan_pdu.extend_from_slice(&response);
            if let Some(ch) = self.rdpsnd_channel {
                if let Err(e) = self.mcs.send_data(ch, &vchan_pdu).await {
                    log::warn!("[rdpsnd] failed to send response: {:?}", e);
                }
            }
        }

        event
    }

    /// Handle data arriving on the drdynvc virtual channel.
    /// Reassembles fragmented channel PDUs then passes the complete payload
    /// to the DrdynvcHandler, returns any decoded bitmaps and audio events.
    async fn handle_drdynvc_data(
        &mut self,
        data: &[u8],
    ) -> (
        Vec<Bitmap>,
        Vec<crate::protocol::rdpsnd::AudioEvent>,
        Option<(u16, u16)>,
    ) {
        const CHANNEL_FLAG_FIRST: u32 = 0x01;
        const CHANNEL_FLAG_LAST: u32 = 0x02;

        if data.len() < 8 {
            log::warn!("[drdynvc] channel data too short: {} bytes", data.len());
            return (vec![], vec![], None);
        }
        let total_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let flags = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let payload = &data[8..];

        if flags & CHANNEL_FLAG_FIRST != 0 {
            self.drdynvc_frag.clear();
            self.drdynvc_frag_total = total_len;
        }
        self.drdynvc_frag.extend_from_slice(payload);

        if flags & CHANNEL_FLAG_LAST == 0 {
            return (vec![], vec![], None);
        }

        let assembled = std::mem::take(&mut self.drdynvc_frag);
        let (bitmaps, responses, audio_events, needs_force_refresh, reset_size) =
            self.drdynvc_handler.process(&assembled);

        // Send any outgoing DRDYNVC PDUs (CAPS response, FRAME_ACK, audio replies, etc.)
        for resp in responses {
            let mut vchan_pdu = Vec::with_capacity(8 + resp.len());
            vchan_pdu.extend_from_slice(&(resp.len() as u32).to_le_bytes());
            let send_flags: u32 = CHANNEL_FLAG_FIRST | CHANNEL_FLAG_LAST;
            vchan_pdu.extend_from_slice(&send_flags.to_le_bytes());
            vchan_pdu.extend_from_slice(&resp);
            if let Some(ch) = self.drdynvc_channel {
                if let Err(e) = self.mcs.send_data(ch, &vchan_pdu).await {
                    log::warn!("[drdynvc] failed to send response: {:?}", e);
                }
            }
        }

        // Request a fresh IDR keyframe if the H264 decoder has lost sync.
        // Match grdp's 2-second rate limit to avoid suppress/allow storms.
        if needs_force_refresh {
            let now = std::time::Instant::now();
            let should_send = match self.last_force_refresh {
                None => true,
                Some(t) => now.duration_since(t).as_secs() >= 2,
            };
            if should_send {
                log::debug!("[client] sending force refresh (suppress→allow) to request IDR");
                if let Err(e) = self.send_force_refresh().await {
                    log::warn!("[client] force refresh failed: {:?}", e);
                } else {
                    self.last_force_refresh = Some(now);
                }
            } else {
                log::debug!("[client] force refresh skipped (rate-limited, last sent <2s ago)");
            }
        }

        (bitmaps, audio_events, reset_size)
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
        let flags = if down {
            btn_flag | PTRFLAGS_DOWN
        } else {
            btn_flag
        };
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
            log::warn!(
                "[bitmap] bitmap_len={} exceeds remaining data, breaking",
                bitmap_len
            );
            break;
        }
        let raw = &data[*pos..*pos + bitmap_len];
        *pos += bitmap_len;

        let pixel_data = if flags & BITMAP_NO_PROCESSING != 0 {
            raw.to_vec()
        } else if flags & BITMAP_COMPRESSION != 0 {
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
        log::warn!(
            "flip_vertical: suspiciously large total={}, skipping",
            total
        );
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

struct FastPathUpdateResult {
    bitmaps: Vec<Bitmap>,
    frame_ids: Vec<u32>,
}

fn parse_fastpath_updates(data: &[u8], frag_buf: &mut Vec<u8>) -> FastPathUpdateResult {
    let mut result = FastPathUpdateResult {
        bitmaps: Vec::new(),
        frame_ids: Vec::new(),
    };
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
            log::warn!(
                "[fastpath] update_code={} frag={} size={} exceeds remaining={}",
                update_code,
                fragmentation,
                size,
                data.len() - pos
            );
            break;
        }
        let update_data = &data[pos..pos + size];
        pos += size;

        log::debug!(
            "[fastpath] update_code=0x{:02x} frag={} size={}",
            update_code,
            fragmentation,
            size
        );

        if update_code != FASTPATH_UPDATETYPE_BITMAP && update_code != FASTPATH_UPDATETYPE_SURFCMDS
        {
            continue;
        }

        // Fragment reassembly (MS-RDPBCGR 2.2.9.1.2.1.2)
        // frag: 0x00=SINGLE, 0x01=LAST, 0x02=FIRST, 0x03=NEXT
        match fragmentation {
            0x00 => {
                parse_fastpath_update_payload(update_code, update_data, &mut result);
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
                parse_fastpath_update_payload(update_code, &reassembled, &mut result);
            }
            _ => {}
        }
    }

    result
}

fn parse_fastpath_update_payload(
    update_code: u8,
    payload: &[u8],
    result: &mut FastPathUpdateResult,
) {
    match update_code {
        FASTPATH_UPDATETYPE_BITMAP => {
            let mut p = 2; // FastPathBitmapUpdateDataPDU header
            let mut rects = parse_bitmap_update(payload, &mut p);
            result.bitmaps.append(&mut rects);
        }
        FASTPATH_UPDATETYPE_SURFCMDS => {
            let (mut rects, mut frame_ids) = parse_surface_commands(payload);
            result.bitmaps.append(&mut rects);
            result.frame_ids.append(&mut frame_ids);
        }
        _ => {}
    }
}

fn parse_surface_commands(data: &[u8]) -> (Vec<Bitmap>, Vec<u32>) {
    let mut pos = 0;
    let mut bitmaps = Vec::new();
    let mut frame_ids = Vec::new();

    while pos + 2 <= data.len() {
        let cmd_type = read_u16_le(data, &mut pos);
        match cmd_type {
            CMDTYPE_SET_SURFACE_BITS | CMDTYPE_STREAM_SURFACE_BITS => {
                if let Some(bitmap) = parse_surface_bits_cmd(data, &mut pos) {
                    bitmaps.push(bitmap);
                } else {
                    break;
                }
            }
            CMDTYPE_FRAME_MARKER => {
                if pos + 6 > data.len() {
                    break;
                }
                let frame_action = read_u16_le(data, &mut pos);
                let frame_id = read_u32_le(data, &mut pos);
                if frame_action == SURFCMD_FRAMEACTION_END {
                    frame_ids.push(frame_id);
                }
            }
            _ => {
                log::warn!("[surface] unknown command type=0x{:04x}", cmd_type);
                break;
            }
        }
    }

    (bitmaps, frame_ids)
}

fn parse_surface_bits_cmd(data: &[u8], pos: &mut usize) -> Option<Bitmap> {
    if *pos + 20 > data.len() {
        return None;
    }

    let dest_left = read_u16_le(data, pos) as i32;
    let dest_top = read_u16_le(data, pos) as i32;
    let dest_right = read_u16_le(data, pos) as i32;
    let dest_bottom = read_u16_le(data, pos) as i32;

    let bpp = read_u8(data, pos) as i32;
    let flags = read_u8(data, pos);
    *pos += 1; // reserved
    let codec_id = read_u8(data, pos);
    let width = read_u16_le(data, pos) as i32;
    let height = read_u16_le(data, pos) as i32;
    let mut bitmap_len = read_u32_le(data, pos) as usize;

    if flags & 0x01 != 0 {
        if *pos + 24 > data.len() || bitmap_len < 24 {
            return None;
        }
        *pos += 24;
        bitmap_len -= 24;
    }

    if *pos + bitmap_len > data.len() {
        log::warn!(
            "[surface] bitmap_len={} exceeds remaining={}",
            bitmap_len,
            data.len() - *pos
        );
        return None;
    }
    let bitmap_data = &data[*pos..*pos + bitmap_len];
    *pos += bitmap_len;

    let (mut pixels, out_bpp) = match codec_id {
        0 => (bitmap_data.to_vec(), bpp),
        1 => (
            decode_nscodec(bitmap_data, width as usize, height as usize)?,
            32,
        ),
        _ => {
            log::warn!("[surface] unsupported codec_id={}", codec_id);
            return None;
        }
    };

    pixels = flip_vertical(&pixels, width as usize, height as usize, out_bpp as usize);

    Some(Bitmap {
        dest_left,
        dest_top,
        dest_right,
        dest_bottom,
        width,
        height,
        bits_per_pixel: out_bpp,
        data: pixels,
    })
}

fn decode_nscodec(data: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    if data.len() < 20 {
        log::warn!("[nscodec] data too short: {}", data.len());
        return None;
    }
    let mut pos = 0;
    let luma_len = read_u32_le(data, &mut pos) as usize;
    let orange_len = read_u32_le(data, &mut pos) as usize;
    let green_len = read_u32_le(data, &mut pos) as usize;
    let alpha_len = read_u32_le(data, &mut pos) as usize;
    let mut color_loss_level = read_u8(data, &mut pos);
    let chroma_subsampling_level = read_u8(data, &mut pos);
    pos += 2; // reserved

    if color_loss_level < 1 {
        color_loss_level = 1;
    }
    let shift = color_loss_level - 1;
    let remaining = &data[pos..];
    let total_plane_len = luma_len
        .saturating_add(orange_len)
        .saturating_add(green_len)
        .saturating_add(alpha_len);
    if total_plane_len > remaining.len() {
        log::warn!("[nscodec] plane lengths exceed data");
        return None;
    }

    let temp_width = (width + 7) & !7;
    let temp_height = (height + 1) & !1;
    let (y_orig_size, co_orig_size, cg_orig_size) = if chroma_subsampling_level > 0 {
        let c_size = (temp_width >> 1) * (temp_height >> 1);
        (temp_width * height, c_size, c_size)
    } else {
        let size = width * height;
        (size, size, size)
    };
    let a_orig_size = width * height;

    let mut off = 0;
    let y_plane = nsc_decompress_plane(&remaining[off..off + luma_len], y_orig_size);
    off += luma_len;
    let co_plane = nsc_decompress_plane(&remaining[off..off + orange_len], co_orig_size);
    off += orange_len;
    let cg_plane = nsc_decompress_plane(&remaining[off..off + green_len], cg_orig_size);
    off += green_len;
    let a_plane = if alpha_len > 0 {
        Some(nsc_decompress_plane(
            &remaining[off..off + alpha_len],
            a_orig_size,
        ))
    } else {
        None
    };

    let mut pixels = vec![0u8; width * height * 4];
    let y_row_width = if chroma_subsampling_level > 0 {
        temp_width
    } else {
        width
    };
    let co_row_width = if chroma_subsampling_level > 0 {
        temp_width >> 1
    } else {
        width
    };

    for py in 0..height {
        let y_row_off = py * y_row_width;
        let mut co_idx = if chroma_subsampling_level > 0 {
            (py >> 1) * co_row_width
        } else {
            py * co_row_width
        };
        let mut cg_idx = co_idx;
        let out_base = py * width;

        for px in 0..width {
            let y_val = y_plane.get(y_row_off + px).copied().unwrap_or(0) as i16;
            let co_val = co_plane
                .get(co_idx)
                .map(|v| ((*v as i16) << shift) as u8 as i8 as i16)
                .unwrap_or(0);
            let cg_val = cg_plane
                .get(cg_idx)
                .map(|v| ((*v as i16) << shift) as u8 as i8 as i16)
                .unwrap_or(0);
            if chroma_subsampling_level == 0 || px % 2 == 1 {
                co_idx += 1;
                cg_idx += 1;
            }

            let off = (out_base + px) * 4;
            pixels[off] = clamp_i16_to_u8(y_val - co_val - cg_val);
            pixels[off + 1] = clamp_i16_to_u8(y_val + cg_val);
            pixels[off + 2] = clamp_i16_to_u8(y_val + co_val - cg_val);
            pixels[off + 3] = a_plane
                .as_ref()
                .and_then(|a| a.get(out_base + px))
                .copied()
                .unwrap_or(0xFF);
        }
    }

    Some(pixels)
}

fn nsc_decompress_plane(input: &[u8], original_size: usize) -> Vec<u8> {
    if input.is_empty() {
        return vec![0xFF; original_size];
    }
    if input.len() >= original_size {
        let mut out = vec![0u8; original_size];
        out.copy_from_slice(&input[..original_size]);
        return out;
    }
    nrle_decode(input, original_size)
}

fn nrle_decode(input: &[u8], original_size: usize) -> Vec<u8> {
    let mut output = vec![0u8; original_size];
    let mut left = original_size;
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;

    while left > 4 && in_pos < input.len() && out_pos < original_size {
        let value = input[in_pos];
        in_pos += 1;

        if left == 5 {
            output[out_pos] = value;
            out_pos += 1;
            left -= 1;
        } else if in_pos < input.len() && value == input[in_pos] {
            in_pos += 1;
            let mut run_len = 0usize;
            if in_pos < input.len() {
                if input[in_pos] < 0xFF {
                    run_len = input[in_pos] as usize + 2;
                    in_pos += 1;
                } else {
                    in_pos += 1;
                    if in_pos + 4 <= input.len() {
                        run_len = u32::from_le_bytes([
                            input[in_pos],
                            input[in_pos + 1],
                            input[in_pos + 2],
                            input[in_pos + 3],
                        ]) as usize;
                        in_pos += 4;
                    }
                }
            }
            run_len = run_len.min(left).min(original_size - out_pos);
            if run_len > 0 {
                output[out_pos] = value;
                let mut wrote = 1usize;
                while wrote < run_len {
                    let step = wrote.min(run_len - wrote);
                    output.copy_within(out_pos..out_pos + step, out_pos + wrote);
                    wrote += step;
                }
                out_pos += run_len;
                left -= run_len;
            }
        } else {
            output[out_pos] = value;
            out_pos += 1;
            left -= 1;
        }
    }

    if left >= 4 && in_pos + 4 <= input.len() && out_pos + 4 <= output.len() {
        output[out_pos..out_pos + 4].copy_from_slice(&input[in_pos..in_pos + 4]);
    }

    output
}

fn clamp_i16_to_u8(v: i16) -> u8 {
    v.clamp(0, 255) as u8
}

/// Returns true if any bitmap in the slice is "large" (area > 300 000 pixels,
/// Returns true if any bitmap is large enough to be considered a meaningful
/// content refresh (as opposed to a small cursor/icon/tooltip update).
/// The threshold (60 000 px ≈ 245×245) is intentionally low so that
/// sub-region force-refresh responses (e.g. 416×240 = 99 840 px for a video
/// area) also flush the stale AVC FIFO.  The C-level guard on
/// `pipeline_elevated` makes this a no-op when no pipeline stall is active.
fn bitmaps_contain_large_refresh(bitmaps: &[crate::bitmap::Bitmap]) -> bool {
    bitmaps
        .iter()
        .any(|b| (b.width as i64) * (b.height as i64) > 60_000)
}
