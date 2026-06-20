use crate::error::RdpError;
use hmac::{Hmac, Mac};
use md4::{Digest as Md4Digest, Md4};
use md5::Md5;

type HmacMd5 = Hmac<Md5>;

// Flags matching grdp's GetNegotiateMessage() (no NEGOTIATE_56, no NEGOTIATE_VERSION)
const NTLM_NEGOTIATE_FLAGS: u32 = 0x40000000 | // NEGOTIATE_KEY_EXCH
    0x20000000 | // NEGOTIATE_128
    0x00080000 | // NEGOTIATE_EXTENDED_SESSIONSECURITY
    0x00008000 | // NEGOTIATE_ALWAYS_SIGN
    0x00000200 | // NEGOTIATE_NTLM
    0x00000020 | // NEGOTIATE_SEAL
    0x00000010 | // NEGOTIATE_SIGN
    0x00000004 | // REQUEST_TARGET
    0x00000001; // NEGOTIATE_UNICODE

/// Stateful RC4 cipher that maintains state across process() calls.
pub struct Rc4Cipher {
    s: Vec<u8>,
    i: usize,
    j: usize,
}

impl Rc4Cipher {
    pub fn new(key: &[u8]) -> Self {
        let mut s: Vec<u8> = (0u8..=255).collect();
        let mut j: usize = 0;
        for i in 0..256 {
            j = (j + s[i] as usize + key[i % key.len()] as usize) % 256;
            s.swap(i, j);
        }
        Rc4Cipher { s, i: 0, j: 0 }
    }

    pub fn process(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        for byte in out.iter_mut() {
            self.i = (self.i + 1) % 256;
            self.j = (self.j + self.s[self.i] as usize) % 256;
            self.s.swap(self.i, self.j);
            *byte ^= self.s[(self.s[self.i] as usize + self.s[self.j] as usize) % 256];
        }
        out
    }
}

/// NTLM security context for encrypting/signing messages (matches grdp's NTLMv2Security).
/// The RC4 state is initialized once and maintained across all gss_encrypt() calls.
pub struct NtlmSecurity {
    encrypt_rc4: Rc4Cipher,
    signing_key: Vec<u8>,
    seq_num: u32,
}

impl NtlmSecurity {
    pub fn new(exported_session_key: &[u8]) -> Self {
        use md5::Digest;
        let sealing_key = Md5::digest(
            [
                exported_session_key,
                b"session key to client-to-server sealing key magic constant\0" as &[u8],
            ]
            .concat(),
        )
        .to_vec();
        let signing_key = Md5::digest(
            [
                exported_session_key,
                b"session key to client-to-server signing key magic constant\0" as &[u8],
            ]
            .concat(),
        )
        .to_vec();
        NtlmSecurity {
            encrypt_rc4: Rc4Cipher::new(&sealing_key),
            signing_key,
            seq_num: 0,
        }
    }

    /// NTLM SEAL matching grdp's GssEncrypt.
    /// Returns: [0x01000000][encrypted_checksum(8)][seqNum(4)][encrypted_message]
    pub fn gss_encrypt(&mut self, message: &[u8]) -> Vec<u8> {
        let encrypted_msg = self.encrypt_rc4.process(message);

        let mut mac_input = self.seq_num.to_le_bytes().to_vec();
        mac_input.extend_from_slice(message);
        let s1 = hmac_md5(&self.signing_key, &mac_input);
        let encrypted_checksum = self.encrypt_rc4.process(&s1[..8]);

        let seq_bytes = self.seq_num.to_le_bytes();
        self.seq_num += 1;

        let mut result = vec![0x01u8, 0x00, 0x00, 0x00];
        result.extend_from_slice(&encrypted_checksum);
        result.extend_from_slice(&seq_bytes);
        result.extend_from_slice(&encrypted_msg);
        result
    }
}

pub struct Ntlm {
    pub domain: String,
    pub user: String,
    pub password: String,
    negotiate_bytes: Vec<u8>,
}

impl Ntlm {
    pub fn new(domain: &str, user: &str, password: &str) -> Self {
        Ntlm {
            domain: domain.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            negotiate_bytes: Vec::new(),
        }
    }

    /// Build NTLM NEGOTIATE message (32 bytes, matching grdp exactly).
    pub fn get_negotiate_message(&mut self) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(b"NTLMSSP\0");
        msg.extend_from_slice(&1u32.to_le_bytes());
        msg.extend_from_slice(&NTLM_NEGOTIATE_FLAGS.to_le_bytes());
        // DomainName: len=0, maxLen=0, offset=0
        msg.extend_from_slice(&[0u8; 8]);
        // Workstation: len=0, maxLen=0, offset=0
        msg.extend_from_slice(&[0u8; 8]);
        // No Version field (NTLMSSP_NEGOTIATE_VERSION not set)
        self.negotiate_bytes = msg.clone();
        msg
    }

    /// Build NTLM AUTHENTICATE message, returning (message_bytes, NtlmSecurity).
    /// Matches grdp's GetAuthenticateMessage closely.
    pub fn get_authenticate_message(
        &self,
        challenge: &[u8],
    ) -> Result<(Vec<u8>, NtlmSecurity), RdpError> {
        if challenge.len() < 56 {
            return Err(RdpError::Auth("NTLM challenge too short".into()));
        }

        let challenge_flags =
            u32::from_le_bytes([challenge[20], challenge[21], challenge[22], challenge[23]]);
        let server_challenge = &challenge[24..32];

        let target_info_len = u16::from_le_bytes([challenge[40], challenge[41]]) as usize;
        let target_info_offset =
            u32::from_le_bytes([challenge[44], challenge[45], challenge[46], challenge[47]])
                as usize;
        let target_info = if target_info_offset + target_info_len <= challenge.len() {
            challenge[target_info_offset..target_info_offset + target_info_len].to_vec()
        } else {
            Vec::new()
        };

        let timestamp_from_server = get_timestamp_from_target_info(&target_info);
        let compute_mic = timestamp_from_server.is_some();
        let timestamp = timestamp_from_server.unwrap_or_else(current_windows_timestamp);

        let client_challenge = generate_random_bytes(8);
        let response_key_nt = ntowfv2(&self.password, &self.user, &self.domain);
        let response_key_lm = response_key_nt.clone(); // LMOWFv2 == NTOWFv2

        // Build NTChallengeResponse (matches grdp's ComputeResponseV2)
        let mut blob = Vec::new();
        blob.extend_from_slice(&[0x01, 0x01]);
        blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        blob.extend_from_slice(&timestamp);
        blob.extend_from_slice(&client_challenge);
        blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        blob.extend_from_slice(&target_info);
        // No trailing Z(4) — matches grdp

        let mut nt_proof_input = server_challenge.to_vec();
        nt_proof_input.extend_from_slice(&blob);
        let nt_proof_str = hmac_md5(&response_key_nt, &nt_proof_input);
        let nt_challenge_response = [nt_proof_str.clone(), blob].concat();

        // LMChallengeResponse = HMAC_MD5(respKeyLM, serverChallenge||clientChallenge) || clientChallenge
        let mut lm_input = server_challenge.to_vec();
        lm_input.extend_from_slice(&client_challenge);
        let lm_hash = hmac_md5(&response_key_lm, &lm_input);
        let lm_challenge_response = [lm_hash, client_challenge].concat(); // 24 bytes

        let session_base_key = hmac_md5(&response_key_nt, &nt_proof_str);
        let exported_session_key = generate_random_bytes(16);
        let encrypted_random_session_key = rc4_crypt(&session_base_key, &exported_session_key);

        let domain_utf16 = to_utf16_le(&self.domain);
        let user_utf16 = to_utf16_le(&self.user);
        // Empty workstation — matches grdp

        // AUTHENTICATE header = 88 bytes: 64 base + 8 Version + 16 MIC (always, matches grdp)
        let fixed_size = 88usize;
        let lm_offset = fixed_size;
        let nt_offset = lm_offset + lm_challenge_response.len();
        let domain_offset = nt_offset + nt_challenge_response.len();
        let user_offset = domain_offset + domain_utf16.len();
        let workstation_offset = user_offset + user_utf16.len();
        let session_key_offset = workstation_offset; // workstation is empty

        let mut msg = Vec::new();
        msg.extend_from_slice(b"NTLMSSP\0");
        msg.extend_from_slice(&3u32.to_le_bytes());
        msg.extend_from_slice(&(lm_challenge_response.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(lm_challenge_response.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(lm_offset as u32).to_le_bytes());
        msg.extend_from_slice(&(nt_challenge_response.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(nt_challenge_response.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(nt_offset as u32).to_le_bytes());
        msg.extend_from_slice(&(domain_utf16.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(domain_utf16.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(domain_offset as u32).to_le_bytes());
        msg.extend_from_slice(&(user_utf16.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(user_utf16.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(user_offset as u32).to_le_bytes());
        msg.extend_from_slice(&0u16.to_le_bytes()); // workstation len=0
        msg.extend_from_slice(&0u16.to_le_bytes()); // workstation maxLen=0
        msg.extend_from_slice(&(workstation_offset as u32).to_le_bytes());
        msg.extend_from_slice(&(encrypted_random_session_key.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(encrypted_random_session_key.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(session_key_offset as u32).to_le_bytes());
        msg.extend_from_slice(&challenge_flags.to_le_bytes()); // mirror server's flags

        // Version (8 bytes): Windows 6.0.6002 if NTLMSSP_NEGOTIATE_VERSION is set
        if challenge_flags & 0x02000000 != 0 {
            msg.push(6);
            msg.push(0);
            msg.extend_from_slice(&6002u16.to_le_bytes());
            msg.push(0);
            msg.push(0);
            msg.push(0);
            msg.push(0x0F);
        } else {
            msg.extend_from_slice(&[0u8; 8]);
        }

        // MIC placeholder at offset 72 (16 zero bytes)
        let mic_offset = msg.len();
        msg.extend_from_slice(&[0u8; 16]);

        // Payload
        msg.extend_from_slice(&lm_challenge_response);
        msg.extend_from_slice(&nt_challenge_response);
        msg.extend_from_slice(&domain_utf16);
        msg.extend_from_slice(&user_utf16);
        // workstation: empty
        msg.extend_from_slice(&encrypted_random_session_key);

        // Compute and inject MIC when server provided timestamp
        if compute_mic {
            let mut mic_input = self.negotiate_bytes.clone();
            mic_input.extend_from_slice(challenge);
            mic_input.extend_from_slice(&msg);
            let mic = hmac_md5(&exported_session_key, &mic_input);
            msg[mic_offset..mic_offset + 16].copy_from_slice(&mic[..16]);
        }

        let security = NtlmSecurity::new(&exported_session_key);
        Ok((msg, security))
    }
}

pub fn to_utf16_le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

pub fn md4_hash(data: &[u8]) -> Vec<u8> {
    let mut hasher = Md4::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

pub fn hmac_md5(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacMd5::new_from_slice(key).expect("HMAC-MD5");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub fn rc4_crypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut cipher = Rc4Cipher::new(key);
    cipher.process(data)
}

pub fn ntowfv2(password: &str, user: &str, domain: &str) -> Vec<u8> {
    let pass_hash = md4_hash(&to_utf16_le(password));
    let mut input = to_utf16_le(&user.to_uppercase());
    input.extend_from_slice(&to_utf16_le(domain));
    hmac_md5(&pass_hash, &input)
}

fn get_timestamp_from_target_info(target_info: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0;
    while pos + 4 <= target_info.len() {
        let av_id = u16::from_le_bytes([target_info[pos], target_info[pos + 1]]);
        let av_len = u16::from_le_bytes([target_info[pos + 2], target_info[pos + 3]]) as usize;
        pos += 4;
        if av_id == 7 && av_len == 8 && pos + 8 <= target_info.len() {
            return Some(target_info[pos..pos + 8].to_vec());
        }
        if av_id == 0 {
            break;
        }
        pos += av_len;
    }
    None
}

fn current_windows_timestamp() -> Vec<u8> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let ft = ns / 100 + 116444736000000000u64;
        ft.to_le_bytes().to_vec()
    }
    #[cfg(target_arch = "wasm32")]
    {
        116444736000000000u64.to_le_bytes().to_vec()
    }
}

fn generate_random_bytes(n: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes).unwrap_or(());
    bytes
}
