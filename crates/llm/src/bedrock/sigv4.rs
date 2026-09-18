use sha2::{Digest, Sha256};

const BLOCK: usize = 64;

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

pub struct SignInput<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(String, String)],
    pub payload: &'a [u8],
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`
    pub amz_date: &'a str,
}

pub fn sign(input: &SignInput<'_>, creds: &Credentials) -> Vec<(String, String)> {
    let date = &input.amz_date[..8];
    let scope = format!("{date}/{}/{}/aws4_request", input.region, input.service);

    let mut norm: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    if let Some(t) = &creds.session_token {
        norm.push(("x-amz-security-token".into(), t.clone()));
    }
    norm.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = norm.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = norm
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let payload_hash = sha256_hex(input.payload);

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method, input.path, input.query, canonical_headers, signed_headers, payload_hash
    );

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        input.amz_date,
        scope,
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, input.region.as_bytes());
    let k_service = hmac_sha256(&k_region, input.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let mut out = vec![(
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            creds.access_key_id, scope, signed_headers, signature
        ),
    )];
    if let Some(t) = &creds.session_token {
        out.push(("x-amz-security-token".into(), t.clone()));
    }
    out
}

pub fn amz_date(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty_matches_the_well_known_digest() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case3() {
        let key = [0xaau8; 20];
        let data = [0xddu8; 50];
        assert_eq!(
            hex(&hmac_sha256(&key, &data)),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn hmac_sha256_handles_oversized_keys() {
        let key = [0xaau8; 131];
        let mac = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn amz_date_formats_epoch_and_known_instants() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(1_700_000_000), "20231114T221320Z");
        assert_eq!(amz_date(1_709_208_000), "20240229T120000Z");
    }

    #[test]
    fn signature_is_deterministic_and_input_sensitive() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let headers = vec![(
            "host".to_string(),
            "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
        )];
        let base = SignInput {
            method: "POST",
            path: "/model/x/converse-stream",
            query: "",
            headers: &headers,
            payload: b"{}",
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20150830T123600Z",
        };

        let a = sign(&base, &creds);
        let b = sign(&base, &creds);
        assert_eq!(a, b);

        let different = SignInput {
            payload: b"{\"a\":1}",
            ..base
        };
        assert_ne!(
            sign(&different, &creds),
            a,
            "changing the payload must change the signature"
        );
    }

    #[test]
    fn session_token_is_signed_and_forwarded() {
        let creds = Credentials {
            access_key_id: "AKID".into(),
            secret_access_key: "SECRET".into(),
            session_token: Some("TOKEN".into()),
        };
        let headers = vec![("host".to_string(), "h".to_string())];
        let out = sign(
            &SignInput {
                method: "POST",
                path: "/",
                query: "",
                headers: &headers,
                payload: b"",
                region: "us-east-1",
                service: "bedrock",
                amz_date: "20150830T123600Z",
            },
            &creds,
        );
        let auth = &out[0].1;
        assert!(
            auth.contains("x-amz-security-token"),
            "the token must go into SignedHeaders, or the server's signature will not match"
        );
        assert_eq!(out[1].0, "x-amz-security-token");
    }
}
