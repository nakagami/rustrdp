use super::ntlm::Ntlm;
use crate::error::RdpError;
use crate::protocol::Transport;

const APP_0: u8 = 0xA0;
const APP_1: u8 = 0xA1;
const APP_2: u8 = 0xA2;
const APP_3: u8 = 0xA3;
const SEQ: u8 = 0x30;
const OCTET_STRING: u8 = 0x04;
const INTEGER: u8 = 0x02;

pub struct Cssp<T: Transport> {
    transport: T,
    ntlm: Ntlm,
}

impl<T: Transport> Cssp<T> {
    pub fn new(transport: T, domain: &str, user: &str, password: &str) -> Self {
        Cssp {
            transport,
            ntlm: Ntlm::new(domain, user, password),
        }
    }

    pub async fn authenticate(&mut self, pub_key: &[u8]) -> Result<(), RdpError> {
        let negotiate = self.ntlm.get_negotiate_message();
        #[cfg(debug_assertions)]
        eprintln!(
            "[CredSSP] sending NTLM NEGOTIATE ({} bytes)",
            negotiate.len()
        );
        let token1 = build_ts_request(2, &negotiate, &[], &[]);
        self.transport.send(&token1).await?;

        #[cfg(debug_assertions)]
        eprintln!("[CredSSP] waiting for NTLM CHALLENGE...");
        let challenge_data = self.recv_ts_request().await?;
        let challenge = extract_token_from_ts_request(&challenge_data)?;
        #[cfg(debug_assertions)]
        eprintln!("[CredSSP] got NTLM CHALLENGE ({} bytes)", challenge.len());

        let (authenticate, mut security) = self.ntlm.get_authenticate_message(&challenge)?;
        let pub_key_auth = security.gss_encrypt(pub_key);
        #[cfg(debug_assertions)]
        eprintln!(
            "[CredSSP] sending NTLM AUTHENTICATE ({} bytes), pubKeyAuth ({} bytes)",
            authenticate.len(),
            pub_key_auth.len()
        );
        let token3 = build_ts_request(2, &authenticate, &pub_key_auth, &[]);
        self.transport.send(&token3).await?;

        #[cfg(debug_assertions)]
        eprintln!("[CredSSP] waiting for server pub key verify...");
        let _server_pub_key_data = self.recv_ts_request().await?;
        #[cfg(debug_assertions)]
        eprintln!("[CredSSP] server pub key verified, sending credentials");

        let credentials =
            build_ts_credentials(&self.ntlm.domain, &self.ntlm.user, &self.ntlm.password);
        let auth_info = security.gss_encrypt(&credentials);
        let token5 = build_ts_request(2, &[], &[], &auth_info);
        self.transport.send(&token5).await?;
        #[cfg(debug_assertions)]
        eprintln!("[CredSSP] authenticate complete");

        Ok(())
    }

    async fn recv_ts_request(&mut self) -> Result<Vec<u8>, RdpError> {
        let tag = self.transport.recv_exact(1).await?[0];
        if tag != SEQ {
            return Err(RdpError::Protocol(format!(
                "CredSSP: expected SEQUENCE (0x30), got 0x{:02x}",
                tag
            )));
        }
        let b0 = self.transport.recv_exact(1).await?[0];
        let (total_len, mut header) = if b0 & 0x80 == 0 {
            (b0 as usize, vec![tag, b0])
        } else {
            let n = (b0 & 0x7F) as usize;
            let len_bytes = self.transport.recv_exact(n).await?;
            let mut len = 0usize;
            for &b in &len_bytes {
                len = (len << 8) | b as usize;
            }
            #[cfg(debug_assertions)]
            eprintln!("[CredSSP] recv_ts_request: n={} len={}", n, len);
            let mut hdr = vec![tag, b0];
            hdr.extend_from_slice(&len_bytes);
            (len, hdr)
        };
        let body = self.transport.recv_exact(total_len).await?;
        #[cfg(debug_assertions)]
        eprintln!(
            "[CredSSP] recv_ts_request: total={} bytes",
            header.len() + body.len()
        );
        header.extend_from_slice(&body);
        Ok(header)
    }

    pub fn into_transport(self) -> T {
        self.transport
    }
}

fn encode_ber_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len < 0x100 {
        vec![0x81, len as u8]
    } else {
        vec![0x82, (len >> 8) as u8, (len & 0xFF) as u8]
    }
}

fn encode_ber_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut buf = vec![tag];
    buf.extend_from_slice(&encode_ber_length(content.len()));
    buf.extend_from_slice(content);
    buf
}

fn build_ts_request(version: i32, token: &[u8], pub_key_auth: &[u8], auth_info: &[u8]) -> Vec<u8> {
    let mut fields = Vec::new();

    let ver_inner = encode_ber_tlv(INTEGER, &[version as u8]);
    fields.extend_from_slice(&encode_ber_tlv(APP_0, &ver_inner));

    if !token.is_empty() {
        let token_data = encode_ber_tlv(OCTET_STRING, token);
        let nego_data = encode_ber_tlv(SEQ, &encode_ber_tlv(APP_0, &token_data));
        let nego_tokens = encode_ber_tlv(SEQ, &nego_data);
        fields.extend_from_slice(&encode_ber_tlv(APP_1, &nego_tokens));
    }

    if !auth_info.is_empty() {
        let auth = encode_ber_tlv(OCTET_STRING, auth_info);
        fields.extend_from_slice(&encode_ber_tlv(APP_2, &auth));
    }

    if !pub_key_auth.is_empty() {
        let pka = encode_ber_tlv(OCTET_STRING, pub_key_auth);
        fields.extend_from_slice(&encode_ber_tlv(APP_3, &pka));
    }

    encode_ber_tlv(SEQ, &fields)
}

fn extract_token_from_ts_request(data: &[u8]) -> Result<Vec<u8>, RdpError> {
    let mut pos = 0;
    if pos >= data.len() || data[pos] != SEQ {
        return Err(RdpError::Protocol("CredSSP: expected SEQUENCE".into()));
    }
    pos += 1;
    let seq_len = decode_ber_length(data, &mut pos)?;
    let end = pos + seq_len;

    while pos < end {
        let tag = data[pos];
        pos += 1;
        let field_len = decode_ber_length(data, &mut pos)?;
        let field_data = &data[pos..pos + field_len];
        if tag == APP_1 {
            let mut p2 = 0;
            if p2 < field_data.len() && field_data[p2] == SEQ {
                p2 += 1;
                let _outer_len = decode_ber_length(field_data, &mut p2)?;
                if p2 < field_data.len() && field_data[p2] == SEQ {
                    p2 += 1;
                    let _inner_len = decode_ber_length(field_data, &mut p2)?;
                    if p2 < field_data.len() && field_data[p2] == APP_0 {
                        p2 += 1;
                        let _app_len = decode_ber_length(field_data, &mut p2)?;
                        if p2 < field_data.len() && field_data[p2] == OCTET_STRING {
                            p2 += 1;
                            let token_len = decode_ber_length(field_data, &mut p2)?;
                            return Ok(field_data[p2..p2 + token_len].to_vec());
                        }
                    }
                }
            }
        }
        pos += field_len;
    }
    Err(RdpError::Protocol("CredSSP: token not found".into()))
}

fn decode_ber_length(data: &[u8], pos: &mut usize) -> Result<usize, RdpError> {
    if *pos >= data.len() {
        return Err(RdpError::Protocol("CredSSP: BER length overflow".into()));
    }
    let b = data[*pos];
    *pos += 1;
    if b & 0x80 == 0 {
        Ok(b as usize)
    } else {
        let n = (b & 0x7F) as usize;
        let mut len = 0usize;
        for _ in 0..n {
            if *pos >= data.len() {
                return Err(RdpError::Protocol("CredSSP: BER length overflow".into()));
            }
            len = (len << 8) | data[*pos] as usize;
            *pos += 1;
        }
        Ok(len)
    }
}

fn build_ts_credentials(domain: &str, user: &str, password: &str) -> Vec<u8> {
    use super::ntlm::to_utf16_le;

    let domain_utf16 = to_utf16_le(domain);
    let user_utf16 = to_utf16_le(user);
    let pass_utf16 = to_utf16_le(password);

    let domain_field = encode_ber_tlv(APP_0, &encode_ber_tlv(OCTET_STRING, &domain_utf16));
    let user_field = encode_ber_tlv(APP_1, &encode_ber_tlv(OCTET_STRING, &user_utf16));
    let pass_field = encode_ber_tlv(APP_2, &encode_ber_tlv(OCTET_STRING, &pass_utf16));

    let mut pw_creds_inner = Vec::new();
    pw_creds_inner.extend_from_slice(&domain_field);
    pw_creds_inner.extend_from_slice(&user_field);
    pw_creds_inner.extend_from_slice(&pass_field);
    let pw_creds = encode_ber_tlv(SEQ, &pw_creds_inner);

    let cred_type = encode_ber_tlv(APP_0, &encode_ber_tlv(INTEGER, &[1u8]));
    let credentials_field = encode_ber_tlv(APP_1, &encode_ber_tlv(OCTET_STRING, &pw_creds));

    let mut ts_creds_inner = Vec::new();
    ts_creds_inner.extend_from_slice(&cred_type);
    ts_creds_inner.extend_from_slice(&credentials_field);
    encode_ber_tlv(SEQ, &ts_creds_inner)
}
