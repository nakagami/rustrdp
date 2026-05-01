// RDPSND protocol handler (MS-RDPEA)
// Implements server-to-client audio over the "rdpsnd" static virtual channel.

// PDU type codes
const SNDC_CLOSE: u8 = 0x01;
const SNDC_WAVE: u8 = 0x02;
const SNDC_WAVECONFIRM: u8 = 0x05;
const SNDC_TRAINING: u8 = 0x06;
const SNDC_FORMATS: u8 = 0x07;
const SNDC_QUALITYMODE: u8 = 0x0C;
const SNDC_WAVE2: u8 = 0x0D;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const TSSNDCAPS_ALIVE: u32 = 0x00000001;
const RDPSND_VERSION_MAJOR: u16 = 0x08;
const DYNAMIC_QUALITY: u16 = 0x0000;

/// Describes a PCM audio stream delivered to the application.
#[derive(Clone)]
pub struct AudioFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

/// A block of decoded audio to be played.
pub struct AudioEvent {
    pub format: AudioFormat,
    pub data: Vec<u8>,
}

#[derive(Clone)]
struct WaveFormat {
    tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    extra_data: Vec<u8>,
}

impl WaveFormat {
    fn pack(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.tag.to_le_bytes());
        b.extend_from_slice(&self.channels.to_le_bytes());
        b.extend_from_slice(&self.samples_per_sec.to_le_bytes());
        b.extend_from_slice(&self.avg_bytes_per_sec.to_le_bytes());
        b.extend_from_slice(&self.block_align.to_le_bytes());
        b.extend_from_slice(&self.bits_per_sample.to_le_bytes());
        b.extend_from_slice(&(self.extra_data.len() as u16).to_le_bytes());
        b.extend_from_slice(&self.extra_data);
        b
    }

    fn unpack(data: &[u8], offset: usize) -> Option<(WaveFormat, usize)> {
        if data.len() < offset + 18 {
            return None;
        }
        let tag = u16::from_le_bytes([data[offset], data[offset + 1]]);
        let channels = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
        let samples_per_sec = u32::from_le_bytes([
            data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7],
        ]);
        let avg_bytes_per_sec = u32::from_le_bytes([
            data[offset + 8], data[offset + 9], data[offset + 10], data[offset + 11],
        ]);
        let block_align = u16::from_le_bytes([data[offset + 12], data[offset + 13]]);
        let bits_per_sample = u16::from_le_bytes([data[offset + 14], data[offset + 15]]);
        let cb_size = u16::from_le_bytes([data[offset + 16], data[offset + 17]]) as usize;
        let end = offset + 18 + cb_size;
        if end > data.len() {
            return None;
        }
        let extra_data = data[offset + 18..end].to_vec();
        Some((
            WaveFormat { tag, channels, samples_per_sec, avg_bytes_per_sec, block_align, bits_per_sample, extra_data },
            end,
        ))
    }
}

/// Stateful RDPSND handler. Feed MCS channel payloads (after virtual-channel
/// fragment reassembly) to `process_data`; send any returned bytes back on the
/// rdpsnd MCS channel.
pub struct RdpsndHandler {
    server_formats: Vec<WaveFormat>,
    client_format_indices: Vec<usize>,
    active_format_index: Option<usize>,
    wave_timestamp: u16,
    wave_block_no: u8,
    pending_wave: Vec<u8>,
    expecting_wave: bool,
}

impl RdpsndHandler {
    pub fn new() -> Self {
        RdpsndHandler {
            server_formats: Vec::new(),
            client_format_indices: Vec::new(),
            active_format_index: None,
            wave_timestamp: 0,
            wave_block_no: 0,
            pending_wave: Vec::new(),
            expecting_wave: false,
        }
    }

    /// Process a reassembled RDPSND PDU. Returns `(response_bytes, Option<AudioEvent>)`.
    /// The caller must send `response_bytes` on the rdpsnd channel.
    pub fn process_data(&mut self, data: &[u8]) -> (Vec<u8>, Option<AudioEvent>) {
        // If we are waiting for the SNDC_WAVE body PDU, the entire next data is the body.
        if self.expecting_wave {
            return self.process_wave_body(data);
        }

        if data.len() < 4 {
            return (Vec::new(), None);
        }

        let msg_type = data[0];
        // data[1] is bPad
        let body_size = u16::from_le_bytes([data[2], data[3]]) as usize;
        let body_end = (4 + body_size).min(data.len());
        let body = &data[4..body_end];

        log::debug!("[rdpsnd] recv msgType=0x{:02x} bodySize={}", msg_type, body_size);

        match msg_type {
            SNDC_FORMATS => self.process_server_formats(body),
            SNDC_TRAINING => self.process_training(body),
            SNDC_WAVE => {
                self.process_wave_info(body);
                (Vec::new(), None)
            }
            SNDC_WAVE2 => self.process_wave2(body),
            SNDC_CLOSE => {
                log::debug!("[rdpsnd] SNDC_CLOSE: audio channel closed by server");
                (Vec::new(), None)
            }
            _ => {
                log::debug!("[rdpsnd] unhandled msgType=0x{:02x}", msg_type);
                (Vec::new(), None)
            }
        }
    }

    fn process_server_formats(&mut self, body: &[u8]) -> (Vec<u8>, Option<AudioEvent>) {
        if body.len() < 20 {
            log::warn!("[rdpsnd] Server Formats PDU too short");
            return (Vec::new(), None);
        }
        let num_formats = u16::from_le_bytes([body[14], body[15]]) as usize;
        let server_version = u16::from_le_bytes([body[17], body[18]]);

        log::debug!("[rdpsnd] Server Formats: version={} numFormats={}", server_version, num_formats);

        let mut offset = 20;
        self.server_formats.clear();
        for i in 0..num_formats {
            match WaveFormat::unpack(body, offset) {
                Some((fmt, new_offset)) => {
                    log::debug!(
                        "[rdpsnd] server format[{}]: tag=0x{:04x} {}Hz {}ch {}bit",
                        i, fmt.tag, fmt.samples_per_sec, fmt.channels, fmt.bits_per_sample
                    );
                    self.server_formats.push(fmt);
                    offset = new_offset;
                }
                None => break,
            }
        }

        // Select PCM formats we support
        self.client_format_indices.clear();
        for (i, f) in self.server_formats.iter().enumerate() {
            if f.tag == WAVE_FORMAT_PCM
                && (f.bits_per_sample == 8 || f.bits_per_sample == 16)
                && (f.channels == 1 || f.channels == 2)
            {
                self.client_format_indices.push(i);
            }
        }

        if self.client_format_indices.is_empty() {
            log::warn!("[rdpsnd] no supported PCM format found");
        }

        let resp = self.build_client_formats(server_version);
        (resp, None)
    }

    fn build_client_formats(&self, server_version: u16) -> Vec<u8> {
        let version = server_version.min(RDPSND_VERSION_MAJOR);

        let mut format_data = Vec::new();
        for &idx in &self.client_format_indices {
            format_data.extend_from_slice(&self.server_formats[idx].pack());
        }

        // Header: dwFlags(4)+dwVolume(4)+dwPitch(4)+wDGramPort(2)+wNumberOfFormats(2)
        //         +cLastBlockConfirmed(1)+wVersion(2)+bPad(1)
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&TSSNDCAPS_ALIVE.to_le_bytes()); // dwFlags
        hdr.extend_from_slice(&0u32.to_le_bytes());            // dwVolume
        hdr.extend_from_slice(&0u32.to_le_bytes());            // dwPitch
        hdr.extend_from_slice(&0u16.to_le_bytes());            // wDGramPort
        hdr.extend_from_slice(&(self.client_format_indices.len() as u16).to_le_bytes());
        hdr.push(0);                                           // cLastBlockConfirmed
        hdr.extend_from_slice(&version.to_le_bytes());        // wVersion
        hdr.push(0);                                           // bPad

        let body: Vec<u8> = hdr.iter().chain(format_data.iter()).cloned().collect();

        let mut pdu = Vec::new();
        pdu.push(SNDC_FORMATS);
        pdu.push(0); // bPad
        pdu.extend_from_slice(&(body.len() as u16).to_le_bytes());
        pdu.extend_from_slice(&body);

        log::debug!("[rdpsnd] sending Client Formats: version={} numFormats={}", version, self.client_format_indices.len());

        // Also append Quality Mode PDU (Windows waits ~10s without it)
        let quality_pdu = self.build_quality_mode();
        let mut out = pdu;
        out.extend_from_slice(&quality_pdu);
        out
    }

    fn build_quality_mode(&self) -> Vec<u8> {
        let mut pdu = Vec::new();
        pdu.push(SNDC_QUALITYMODE);
        pdu.push(0); // bPad
        pdu.extend_from_slice(&4u16.to_le_bytes()); // bodySize
        pdu.extend_from_slice(&DYNAMIC_QUALITY.to_le_bytes()); // wQualityMode
        pdu.extend_from_slice(&0u16.to_le_bytes()); // Reserved
        pdu
    }

    fn process_training(&self, body: &[u8]) -> (Vec<u8>, Option<AudioEvent>) {
        if body.len() < 4 {
            return (Vec::new(), None);
        }
        let timestamp = u16::from_le_bytes([body[0], body[1]]);
        let pack_size = u16::from_le_bytes([body[2], body[3]]);
        log::debug!("[rdpsnd] Training: ts={} packSize={}", timestamp, pack_size);

        let mut pdu = Vec::new();
        pdu.push(SNDC_TRAINING);
        pdu.push(0); // bPad
        pdu.extend_from_slice(&4u16.to_le_bytes()); // bodySize
        pdu.extend_from_slice(&timestamp.to_le_bytes());
        pdu.extend_from_slice(&pack_size.to_le_bytes());

        log::debug!("[rdpsnd] sent Training Confirm");
        (pdu, None)
    }

    fn process_wave_info(&mut self, body: &[u8]) {
        if body.len() < 12 {
            log::warn!("[rdpsnd] WaveInfo body too short");
            return;
        }
        let timestamp = u16::from_le_bytes([body[0], body[1]]);
        let format_no = u16::from_le_bytes([body[2], body[3]]) as usize;
        let block_no = body[4];
        let initial_data = body[8..12].to_vec(); // 4 bytes carried into next PDU

        self.wave_timestamp = timestamp;
        self.wave_block_no = block_no;
        self.pending_wave = initial_data;

        if format_no < self.client_format_indices.len() {
            self.active_format_index = Some(self.client_format_indices[format_no]);
        } else {
            log::warn!("[rdpsnd] WaveInfo format index {} out of range (max {})", format_no, self.client_format_indices.len());
        }

        self.expecting_wave = true;
        log::debug!("[rdpsnd] WaveInfo: ts={} fmt={} block={}", timestamp, format_no, block_no);
    }

    fn process_wave_body(&mut self, data: &[u8]) -> (Vec<u8>, Option<AudioEvent>) {
        self.expecting_wave = false;
        // First 4 bytes of wave body duplicate the WaveInfo header (padding); skip them.
        let audio_data: Vec<u8> = self.pending_wave.iter()
            .chain(if data.len() > 4 { data[4..].iter() } else { [].iter() })
            .cloned()
            .collect();
        self.pending_wave.clear();

        log::debug!("[rdpsnd] Wave body: {} bytes audio", audio_data.len());

        let confirm = self.build_wave_confirm(self.wave_timestamp, self.wave_block_no);
        let event = self.make_audio_event(audio_data);
        (confirm, event)
    }

    fn process_wave2(&mut self, body: &[u8]) -> (Vec<u8>, Option<AudioEvent>) {
        if body.len() < 12 {
            log::warn!("[rdpsnd] Wave2 body too short");
            return (Vec::new(), None);
        }
        let timestamp = u16::from_le_bytes([body[0], body[1]]);
        let format_no = u16::from_le_bytes([body[2], body[3]]) as usize;
        let block_no = body[4];
        let audio_data = body[12..].to_vec();

        if format_no < self.client_format_indices.len() {
            self.active_format_index = Some(self.client_format_indices[format_no]);
        } else {
            log::warn!("[rdpsnd] Wave2 format index {} out of range", format_no);
        }

        log::debug!("[rdpsnd] Wave2: ts={} fmt={} block={} dataLen={}", timestamp, format_no, block_no, audio_data.len());

        let confirm = self.build_wave_confirm(timestamp, block_no);
        let event = self.make_audio_event(audio_data);
        (confirm, event)
    }

    fn build_wave_confirm(&self, timestamp: u16, block_no: u8) -> Vec<u8> {
        let mut pdu = Vec::new();
        pdu.push(SNDC_WAVECONFIRM);
        pdu.push(0); // bPad
        pdu.extend_from_slice(&4u16.to_le_bytes()); // bodySize
        pdu.extend_from_slice(&timestamp.to_le_bytes());
        pdu.push(block_no);
        pdu.push(0); // bPad
        pdu
    }

    fn make_audio_event(&self, data: Vec<u8>) -> Option<AudioEvent> {
        let idx = self.active_format_index?;
        if idx >= self.server_formats.len() {
            return None;
        }
        let f = &self.server_formats[idx];
        Some(AudioEvent {
            format: AudioFormat {
                channels: f.channels,
                sample_rate: f.samples_per_sec,
                bits_per_sample: f.bits_per_sample,
            },
            data,
        })
    }
}
