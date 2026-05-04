/// DRDYNVC (MS-RDPEDYC) dynamic virtual channel handler.
///
/// Receives raw PDU bytes from the static "drdynvc" channel,
/// multiplexes dynamic channels, routes RDPGFX messages, and
/// returns decoded bitmaps plus outgoing PDUs to send back.

use std::collections::HashMap;
use crate::bitmap::Bitmap;
use crate::protocol::rdpgfx::RdpgfxHandler;
use crate::protocol::rdpsnd::{RdpsndHandler, AudioEvent};

// ── DYNVC message commands ────────────────────────────────────────────────────
const CMD_CREATE_REQ:        u8 = 0x01;
const CMD_DATA_FIRST:        u8 = 0x02;
const CMD_DATA:              u8 = 0x03;
const CMD_CLOSE:             u8 = 0x04;
const CMD_CAPABILITIES:      u8 = 0x05;
const CMD_SOFT_SYNC_REQUEST: u8 = 0x08;

const GFX_CHANNEL_NAME:         &str = "Microsoft::Windows::RDS::Graphics";
const AUDIO_DVC_CHANNEL_NAME:   &str = "AUDIO_PLAYBACK_DVC";
const AUDIO_LOSSY_CHANNEL_NAME: &str = "AUDIO_PLAYBACK_LOSSY_DVC";

/// VOR (Video Optimized Remoting) channels that rustrdp does not implement.
/// Rejecting these forces the server to keep sending video via RDPGFX.
/// Without rejection, the server silently switches to VOR during video
/// playback or window transitions, causing the screen to freeze.
const REJECTED_CHANNELS: &[&str] = &[
    "Microsoft::Windows::RDS::Video::Control::v08.01",
    "Microsoft::Windows::RDS::Video::Data::v08.01",
    "Microsoft::Windows::RDS::Geometry::v08.01",
];

/// In-progress reassembly for a fragmented DVC message.
struct Fragment {
    buf:      Vec<u8>,
    expected: usize,
}

enum DvcChannel {
    Gfx(RdpgfxHandler),
    Audio(RdpsndHandler),
    Unknown,
}

pub struct DrdynvcHandler {
    channels:   HashMap<u32, DvcChannel>,
    fragments:  HashMap<u32, Fragment>,
    server_version: u16,
    /// Optional AVC decoder plug-in, given away to the first GFX channel created.
    avc_dec: Option<Box<dyn crate::avc::AvcDecoder>>,
}

impl DrdynvcHandler {
    pub fn new() -> Self {
        DrdynvcHandler {
            channels:  HashMap::new(),
            fragments: HashMap::new(),
            server_version: 1,
            avc_dec: None,
        }
    }

    /// Inject an AVC decoder that will be passed to the RDPGFX channel handler
    /// when the server creates the GFX dynamic virtual channel.
    pub fn set_avc_decoder(&mut self, dec: Option<Box<dyn crate::avc::AvcDecoder>>) {
        self.avc_dec = dec;
    }

    /// Process one DVC PDU.
    /// Returns (bitmaps produced, raw DRDYNVC PDUs to send back to server, audio events, needs_force_refresh, reset_size).
    /// `needs_force_refresh` is true when the H264 decoder needs an IDR keyframe.
    pub fn process(&mut self, data: &[u8]) -> (Vec<Bitmap>, Vec<Vec<u8>>, Vec<AudioEvent>, bool, Option<(u16, u16)>) {
        if data.is_empty() {
            return (vec![], vec![], vec![], false, None);
        }
        let header   = data[0];
        let cmd      = (header >> 4) & 0x0F;
        let sp       = (header >> 2) & 0x03;
        let cb_ch_id = header & 0x03;

        let mut bitmaps  = Vec::new();
        let mut outgoing = Vec::new();
        let mut audio    = Vec::new();
        let mut force_refresh = false;
        let mut reset_size = None;

        match cmd {
            CMD_CAPABILITIES => {
                self.handle_capabilities(data, &mut outgoing);
            }
            CMD_CREATE_REQ => {
                self.handle_create(data, cb_ch_id, &mut outgoing);
            }
            CMD_DATA_FIRST => {
                self.handle_data_first(data, cb_ch_id, sp, &mut bitmaps, &mut outgoing, &mut audio, &mut force_refresh, &mut reset_size);
            }
            CMD_DATA => {
                self.handle_data(data, cb_ch_id, &mut bitmaps, &mut outgoing, &mut audio, &mut force_refresh, &mut reset_size);
            }
            CMD_CLOSE => {
                let ch_id = read_ch_id(data, 1, cb_ch_id);
                self.channels.remove(&ch_id);
                self.fragments.remove(&ch_id);
                log::debug!("[drdynvc] CLOSE ch={}", ch_id);
            }
            CMD_SOFT_SYNC_REQUEST => {
                outgoing.push(build_soft_sync_response());
            }
            _ => {
                log::debug!("[drdynvc] unhandled cmd={}", cmd);
            }
        }

        (bitmaps, outgoing, audio, force_refresh, reset_size)
    }

    /// Called when a large/full-screen raw Bitmap Update arrives, indicating
    /// the server responded to a SuppressOutput force-refresh PDU.  Passes
    /// the signal through to the active RDPGFX channel so it can flush the
    /// stale AVC pipeline and region FIFO before those stale frames overwrite
    /// the freshly refreshed pixels.
    pub fn signal_screen_refreshed(&mut self) {
        for channel in self.channels.values_mut() {
            if let DvcChannel::Gfx(gfx) = channel {
                gfx.signal_screen_refreshed();
            }
        }
    }

    // ── CAPABILITIES ──────────────────────────────────────────────────────────

    fn handle_capabilities(&mut self, data: &[u8], out: &mut Vec<Vec<u8>>) {
        // Header(1) + Pad(1) + Version(2)
        if data.len() < 4 { return; }
        self.server_version = u16::from_le_bytes([data[2], data[3]]);
        log::debug!("[drdynvc] CAPABILITIES server_version={}", self.server_version);
        let version = self.server_version.min(3);
        let mut pdu = vec![
            0x50u8,       // Cmd=5 (CAPABILITIES) | Sp=0 | CbChId=0
            0x00,         // pad
            (version & 0xFF) as u8,
            ((version >> 8) & 0xFF) as u8,
        ];
        // Add padding to match standard 4-byte body
        drop(pdu.drain(..)); // clear and rebuild to be explicit
        pdu.push(0x50);
        pdu.push(0x00);
        pdu.extend_from_slice(&version.to_le_bytes());
        out.push(pdu);
    }

    // ── CREATE_REQ ────────────────────────────────────────────────────────────

    fn handle_create(&mut self, data: &[u8], cb_ch_id: u8, out: &mut Vec<Vec<u8>>) {
        let id_bytes = ch_id_len(cb_ch_id);
        if data.len() < 1 + id_bytes { return; }
        let ch_id = read_ch_id(data, 1, cb_ch_id);

        // Channel name is null-terminated UTF-8 after the channel ID
        let name_start = 1 + id_bytes;
        let name = read_cstring(&data[name_start..]);
        log::debug!("[drdynvc] CREATE_REQ ch={} name={}", ch_id, name);

        // Reject VOR channels: send E_FAIL so the server keeps using RDPGFX.
        if REJECTED_CHANNELS.contains(&name.as_str()) {
            log::info!("[drdynvc] rejecting VOR channel: {}", name);
            out.push(build_create_rsp(ch_id, cb_ch_id, 0x80004005)); // E_FAIL
            return;
        }

        // Send CREATE_RESPONSE (status=0 = S_OK).
        out.push(build_create_rsp(ch_id, cb_ch_id, 0));

        if name == GFX_CHANNEL_NAME {
            // Take the AVC decoder (if any) and give it to the GFX handler.
            // `take()` moves the decoder to the handler; any subsequent
            // CREATE_REQ for the GFX channel (e.g. a server-initiated
            // session reset) will create a new handler without a decoder.
            // In practice this edge case is rare: the GFX channel lifecycle
            // follows the RDP session lifecycle and the decoder holds no
            // per-session state that cannot be recovered by the codec reset
            // path (RESET_GRAPHICS → AvcDecoder::reset).  If future use
            // cases require decoder reuse across channel recreations, the
            // factory pattern (Box<dyn Fn() -> Box<dyn AvcDecoder>>) can be
            // introduced here without changing the public API.
            let avc_dec = self.avc_dec.take();
            let mut gfx = RdpgfxHandler::with_avc(avc_dec);
            // Build CAPS_ADVERTISE and wrap it in a DATA PDU
            let caps_pdus = gfx.on_channel_created();
            self.channels.insert(ch_id, DvcChannel::Gfx(gfx));
            for pdu in caps_pdus {
                out.push(wrap_data_pdu(ch_id, cb_ch_id, &pdu));
            }
        } else if name == AUDIO_DVC_CHANNEL_NAME || name == AUDIO_LOSSY_CHANNEL_NAME {
            log::debug!("[drdynvc] registering audio DVC channel: {}", name);
            self.channels.insert(ch_id, DvcChannel::Audio(RdpsndHandler::new()));
        } else {
            self.channels.insert(ch_id, DvcChannel::Unknown);
        }
    }

    // ── DATA_FIRST ────────────────────────────────────────────────────────────

    fn handle_data_first(
        &mut self, data: &[u8], cb_ch_id: u8, sp: u8,
        bitmaps: &mut Vec<Bitmap>, out: &mut Vec<Vec<u8>>, audio: &mut Vec<AudioEvent>,
        force_refresh: &mut bool,
        reset_size: &mut Option<(u16, u16)>,
    ) {
        let id_bytes  = ch_id_len(cb_ch_id);
        let len_bytes = len_field_len(sp);
        let header_sz = 1 + id_bytes + len_bytes;
        if data.len() < header_sz { return; }

        let ch_id = read_ch_id(data, 1, cb_ch_id);
        let total = read_len(&data[1 + id_bytes..], sp) as usize;
        let payload = &data[header_sz..];

        if total == payload.len() {
            // Single-packet message (total matches first chunk)
            self.dispatch_channel_data(ch_id, payload, bitmaps, out, audio, force_refresh, reset_size);
        } else {
            let mut frag = Fragment { buf: Vec::with_capacity(total), expected: total };
            frag.buf.extend_from_slice(payload);
            self.fragments.insert(ch_id, frag);
        }
    }

    // ── DATA ──────────────────────────────────────────────────────────────────

    fn handle_data(
        &mut self, data: &[u8], cb_ch_id: u8,
        bitmaps: &mut Vec<Bitmap>, out: &mut Vec<Vec<u8>>, audio: &mut Vec<AudioEvent>,
        force_refresh: &mut bool,
        reset_size: &mut Option<(u16, u16)>,
    ) {
        let id_bytes = ch_id_len(cb_ch_id);
        if data.len() < 1 + id_bytes { return; }
        let ch_id  = read_ch_id(data, 1, cb_ch_id);
        let payload = &data[1 + id_bytes..];

        // Reassemble or dispatch directly
        let complete = if let Some(frag) = self.fragments.get_mut(&ch_id) {
            frag.buf.extend_from_slice(payload);
            frag.buf.len() >= frag.expected
        } else {
            // No fragment in progress → single-shot data
            self.dispatch_channel_data(ch_id, payload, bitmaps, out, audio, force_refresh, reset_size);
            return;
        };

        if complete {
            let buf = self.fragments.remove(&ch_id).unwrap().buf;
            self.dispatch_channel_data(ch_id, &buf, bitmaps, out, audio, force_refresh, reset_size);
        }
    }

    // ── Channel dispatch ──────────────────────────────────────────────────────

    fn dispatch_channel_data(
        &mut self, ch_id: u32, data: &[u8],
        bitmaps: &mut Vec<Bitmap>, out: &mut Vec<Vec<u8>>, audio: &mut Vec<AudioEvent>,
        force_refresh: &mut bool,
        reset_size: &mut Option<(u16, u16)>,
    ) {
        let cb_ch_id = ch_id_size(ch_id);
        match self.channels.get_mut(&ch_id) {
            Some(DvcChannel::Gfx(gfx)) => {
                let (new_bitmaps, replies, fr, rs) = gfx.process(data);
                bitmaps.extend(new_bitmaps);
                if fr { *force_refresh = true; }
                if rs.is_some() { *reset_size = rs; }
                for r in replies {
                    out.push(wrap_data_pdu(ch_id, cb_ch_id, &r));
                }
            }
            Some(DvcChannel::Audio(rdpsnd)) => {
                let (response, event) = rdpsnd.process_data(data);
                if !response.is_empty() {
                    out.push(wrap_data_pdu(ch_id, cb_ch_id, &response));
                }
                if let Some(ev) = event {
                    audio.push(ev);
                }
            }
            Some(DvcChannel::Unknown) | None => {}
        }
    }
}

// ── Encoding helpers ──────────────────────────────────────────────────────────

/// Number of bytes used for the channel ID field.
fn ch_id_len(cb_ch_id: u8) -> usize {
    match cb_ch_id { 0 => 1, 1 => 2, _ => 4 }
}

/// Number of bytes used for the length field (DATA_FIRST only).
fn len_field_len(sp: u8) -> usize {
    match sp { 0 => 1, 1 => 2, _ => 4 }
}

/// Read a channel ID from `data[offset..]` based on `cb_ch_id`.
fn read_ch_id(data: &[u8], offset: usize, cb_ch_id: u8) -> u32 {
    let len = ch_id_len(cb_ch_id);
    if offset + len > data.len() { return 0; }
    match len {
        1 => data[offset] as u32,
        2 => u16::from_le_bytes([data[offset], data[offset + 1]]) as u32,
        _ => u32::from_le_bytes([
            data[offset], data[offset + 1],
            data[offset + 2], data[offset + 3],
        ]),
    }
}

/// Read a length field from `data[0..]` based on `sp`.
fn read_len(data: &[u8], sp: u8) -> u32 {
    match sp {
        0 if !data.is_empty() => data[0] as u32,
        1 if data.len() >= 2  => u16::from_le_bytes([data[0], data[1]]) as u32,
        2 if data.len() >= 4  => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
        _ => 0,
    }
}

/// Read a null-terminated UTF-8 string.
fn read_cstring(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// Determine cb_ch_id encoding for a given channel ID.
fn ch_id_size(ch_id: u32) -> u8 {
    if ch_id <= 0xFF { 0 } else if ch_id <= 0xFFFF { 1 } else { 2 }
}

/// Write a channel ID into a buffer, returning the number of bytes written.
fn write_ch_id(buf: &mut Vec<u8>, ch_id: u32, cb_ch_id: u8) {
    match ch_id_len(cb_ch_id) {
        1 => buf.push(ch_id as u8),
        2 => buf.extend_from_slice(&(ch_id as u16).to_le_bytes()),
        _ => buf.extend_from_slice(&ch_id.to_le_bytes()),
    }
}

/// Wrap an RDPGFX PDU as a DYNVC_DATA PDU.
fn wrap_data_pdu(ch_id: u32, cb_ch_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(1 + ch_id_len(cb_ch_id) + payload.len());
    let header = (CMD_DATA << 4) | (cb_ch_id & 0x03);
    pdu.push(header);
    write_ch_id(&mut pdu, ch_id, cb_ch_id);
    pdu.extend_from_slice(payload);
    pdu
}

/// Build SOFT_SYNC_RESPONSE PDU.
fn build_soft_sync_response() -> Vec<u8> {
    // Cmd=9, Sp=0, CbChId=0
    vec![0x90, 0x00, 0x00, 0x00, 0x00]
}

/// Build DYNVC_CREATE_RSP PDU.
/// Header Cmd=1 (same as CREATE_REQ), direction implies it's the response.
fn build_create_rsp(ch_id: u32, cb_ch_id: u8, status: u32) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(1 + ch_id_len(cb_ch_id) + 4);
    let header = (0x01u8 << 4) | (cb_ch_id & 0x03);
    pdu.push(header);
    write_ch_id(&mut pdu, ch_id, cb_ch_id);
    pdu.extend_from_slice(&status.to_le_bytes());
    pdu
}
