use crate::error::RdpError;
use md4::{Md4, Digest as Md4Digest};
use hmac::{Hmac, Mac};
use md5::Md5;

type HmacMd5 = Hmac<Md5>;

const NTLM_NTLMv2_FLAGS: u32 =
    0x80000000 | // NEGOTIATE_56
    0x40000000 | // NEGOTIATE_KEY_EXCH
    0x20000000 | // NEGOTIATE_128
    0x00080000 | // NEGOTIATE_EXTENDED_SESSIONSECURITY
    0x00008000 | // NEGOTIATE_ALWAYS_SIGN
    0x00000200 | // NEGOTIATE_NTLM
    0x00000020 | // NEGOTIATE_SEAL
    0x00000010 | // NEGOTIATE_SIGN
    0x00000004 | // REQUEST_TARGET
    0x00000001;  // NEGOTIATE_UNICODE

pub struct Ntlm {
    pub domain: String,
    pub user: String,
    pub password: String,
}

impl Ntlm {
    pub fn new(domain: &str, user: &str, password: &str) -> Self {
        Ntlm {
            domain: domain.to_string(),
            user: user.to_string(),
            password: password.to_string(),
        }
    }

    pub fn get_negotiate_message(&self) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(b"NTLMSSP\0");
        msg.extend_from_slice(&1u32.to_le_bytes());
        msg.extend_from_slice(&NTLM_NTLMv2_FLAGS.to_le_bytes());
        msg.extend_from_slice(&0u16.to_le_bytes());
        msg.extend_from_slice(&0u16.to_le_bytes());
        msg.extend_from_slice(&32u32.to_le_bytes());
        msg.extend_from_slice(&0u16.to_le_bytes());
        msg.extend_from_slice(&0u16.to_le_bytes());
        msg.extend_from_slice(&32u32.to_le_bytes());
        msg.push(10);
        msg.push(0);
        msg.extend_from_slice(&0u16.to_le_bytes());
        msg.push(0); msg.push(0); msg.push(0);
        msg.push(0x0F);
        msg
    }

    pub fn get_authenticate_message(&self, challenge: &[u8]) -> Result<Vec<u8>, RdpError> {
        if challenge.len() < 56 {
            return Err(RdpError::Auth("NTLM challenge too short".into()));
        }

        let server_challenge = &challenge[24..32];

        let target_info_len = u16::from_le_bytes([challenge[40], challenge[41]]) as usize;
        let target_info_offset = u32::from_le_bytes([challenge[44], challenge[45], challenge[46], challenge[47]]) as usize;
        let target_info = if target_info_offset + target_info_len <= challenge.len() {
            challenge[target_info_offset..target_info_offset + target_info_len].to_vec()
        } else {
            Vec::new()
        };

        let timestamp = get_timestamp_from_target_info(&target_info).unwrap_or_else(|| {
            let ft: u64 = 116444736000000000u64;
            ft.to_le_bytes().to_vec()
        });

        let client_challenge = generate_random_bytes(8);
        let response_key_nt = ntowfv2(&self.password, &self.user, &self.domain);

        let mut blob = Vec::new();
        blob.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);
        blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        blob.extend_from_slice(&timestamp);
        blob.extend_from_slice(&client_challenge);
        blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        blob.extend_from_slice(&target_info);
        blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut nt_proof_input = server_challenge.to_vec();
        nt_proof_input.extend_from_slice(&blob);
        let nt_proof_str = hmac_md5(&response_key_nt, &nt_proof_input);

        let nt_challenge_response = [nt_proof_str.clone(), blob].concat();

        let session_base_key = hmac_md5(&response_key_nt, &nt_proof_str);

        let exported_session_key = generate_random_bytes(16);
        let encrypted_random_session_key = rc4_crypt(&session_base_key, &exported_session_key);

        let domain_utf16 = to_utf16_le(&self.domain);
        let user_utf16 = to_utf16_le(&self.user);
        let workstation_utf16 = to_utf16_le("WORKSTATION");
        let lm_response = vec![0u8; 24];

        let fixed_size = 72usize;
        let lm_offset = fixed_size;
        let nt_offset = lm_offset + lm_response.len();
        let domain_offset = nt_offset + nt_challenge_response.len();
        let user_offset = domain_offset + domain_utf16.len();
        let workstation_offset = user_offset + user_utf16.len();
        let session_key_offset = workstation_offset + workstation_utf16.len();

        let mut msg = Vec::new();
        msg.extend_from_slice(b"NTLMSSP\0");
        msg.extend_from_slice(&3u32.to_le_bytes());
        msg.extend_from_slice(&(lm_response.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(lm_response.len() as u16).to_le_bytes());
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
        msg.extend_from_slice(&(workstation_utf16.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(workstation_utf16.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(workstation_offset as u32).to_le_bytes());
        msg.extend_from_slice(&(encrypted_random_session_key.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(encrypted_random_session_key.len() as u16).to_le_bytes());
        msg.extend_from_slice(&(session_key_offset as u32).to_le_bytes());
        msg.extend_from_slice(&NTLM_NTLMv2_FLAGS.to_le_bytes());
        msg.extend_from_slice(&lm_response);
        msg.extend_from_slice(&nt_challenge_response);
        msg.extend_from_slice(&domain_utf16);
        msg.extend_from_slice(&user_utf16);
        msg.extend_from_slice(&workstation_utf16);
        msg.extend_from_slice(&encrypted_random_session_key);

        Ok(msg)
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
    let mut s: Vec<u8> = (0u8..=255).collect();
    let mut j: usize = 0;
    for i in 0..256 {
        j = (j + s[i] as usize + key[i % key.len()] as usize) % 256;
        s.swap(i, j);
    }
    let mut i = 0usize;
    let mut j = 0usize;
    let mut out = data.to_vec();
    for byte in out.iter_mut() {
        i = (i + 1) % 256;
        j = (j + s[i] as usize) % 256;
        s.swap(i, j);
        *byte ^= s[(s[i] as usize + s[j] as usize) % 256];
    }
    out
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
        if av_id == 0 { break; }
        pos += av_len;
    }
    None
}

fn generate_random_bytes(n: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes).unwrap_or(());
    bytes
}
