//! GameStream 페어링/스트림에 필요한 암호 프리미티브.
//!
//! 참조 구현: hgaiser/moonshine (BSD-2), moonlight-common-c, Sunshine.
//! 순수-Rust 크레이트만 사용 (aws-lc-rs/cmake/nasm/libclang 불필요).
//!
//! - 페어링 챌린지: AES-128-**ECB** (블록별, 패딩 없음; moonlight의 특이점)
//! - 제어 채널: AES-128-**GCM**
//! - 오디오: AES-128-**CBC** (PKCS7)
//! - 서명: RSA PKCS#1 v1.5 + SHA-256
//! - 키 유도: SHA-256(salt || pin)[..16]

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, Key, Nonce};
use anyhow::{anyhow, bail, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256 as RsaSha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::RsaPrivateKey;
use sha2::{Digest, Sha256};

/// SHA-256(salt || pin)의 앞 16바이트를 AES 키로 사용.
pub fn derive_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(pin.as_bytes());
    let hash = hasher.finalize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&hash[..16]);
    key
}

/// AES-128-ECB 암호화 (블록별, 패딩 없음). 입력은 16의 배수여야 함.
pub fn aes_ecb_encrypt(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    if data.len() % 16 != 0 {
        bail!("ECB input length {} not a multiple of 16", data.len());
    }
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.encrypt_block(block.into());
    }
    Ok(out)
}

/// AES-128-ECB 복호화 (블록별, 패딩 없음).
pub fn aes_ecb_decrypt(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    if data.len() % 16 != 0 {
        bail!("ECB input length {} not a multiple of 16", data.len());
    }
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.decrypt_block(block.into());
    }
    Ok(out)
}

/// AES-128-GCM 암호화. `tag`(16바이트)에 인증 태그를 출력하고 순수 ciphertext 반환.
pub fn aes_gcm_encrypt(plaintext: &[u8], key: &[u8; 16], iv: &[u8], tag: &mut [u8; 16]) -> Result<Vec<u8>> {
    let key = Key::<Aes128Gcm>::from_slice(key);
    let cipher = Aes128Gcm::new(key);
    let nonce = Nonce::from_slice(iv);
    let mut ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow!("gcm encrypt: {e}"))?;
    // aes-gcm은 ciphertext 끝에 태그를 붙인다. 분리한다.
    let split = ct.len() - 16;
    tag.copy_from_slice(&ct[split..]);
    ct.truncate(split);
    Ok(ct)
}

/// AES-128-GCM 복호화. ciphertext + 별도 tag → plaintext.
pub fn aes_gcm_decrypt(ciphertext: &[u8], key: &[u8; 16], iv: &[u8], tag: &[u8; 16]) -> Result<Vec<u8>> {
    let key = Key::<Aes128Gcm>::from_slice(key);
    let cipher = Aes128Gcm::new(key);
    let nonce = Nonce::from_slice(iv);
    let mut payload = Vec::with_capacity(ciphertext.len() + 16);
    payload.extend_from_slice(ciphertext);
    payload.extend_from_slice(tag);
    cipher
        .decrypt(nonce, payload.as_ref())
        .map_err(|e| anyhow!("gcm decrypt: {e}"))
}

/// AES-128-CBC 암호화 (PKCS7 패딩).
pub fn aes_cbc_encrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    type Enc = cbc::Encryptor<Aes128>;
    let enc = Enc::new(key.into(), iv.into());
    Ok(enc.encrypt_padded_vec_mut::<Pkcs7>(data))
}

/// RSA PKCS#1 v1.5 + SHA-256 서명. `key_pem`은 PKCS#8 PEM.
pub fn rsa_sign_sha256(data: &[u8], key_pem: &str) -> Result<Vec<u8>> {
    let private_key =
        RsaPrivateKey::from_pkcs8_pem(key_pem).map_err(|e| anyhow!("parse private key pem: {e}"))?;
    let signing_key = SigningKey::<RsaSha256>::new(private_key);
    let sig = signing_key.sign(data);
    Ok(sig.to_vec())
}

/// 클라이언트 인증서(PEM)의 공개키로 RSA PKCS#1 v1.5 + SHA-256 서명 검증.
pub fn rsa_verify_cert_sha256(cert_pem: &str, data: &[u8], signature: &[u8]) -> Result<()> {
    use x509_parser::prelude::*;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|e| anyhow!("parse client pem: {e}"))?;
    let cert = pem.parse_x509().map_err(|e| anyhow!("parse client x509: {e}"))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let public_key = rsa::RsaPublicKey::from_public_key_der(spki_der)
        .map_err(|e| anyhow!("extract rsa public key: {e}"))?;
    let verifying_key = VerifyingKey::<RsaSha256>::new(public_key);
    let sig = Signature::try_from(signature).map_err(|e| anyhow!("parse signature: {e}"))?;
    verifying_key
        .verify(data, &sig)
        .map_err(|e| anyhow!("client signature verification failed: {e}"))
}

use rsa::pkcs8::DecodePublicKey;

/// X.509 인증서 PEM의 서명값(signatureValue 비트스트링) 추출.
/// moonlight 챌린지에서 서버/클라이언트 인증서의 signature 필드를 해시 재료로 쓴다.
pub fn cert_signature(cert_pem: &str) -> Result<Vec<u8>> {
    use x509_parser::prelude::*;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|e| anyhow!("parse pem: {e}"))?;
    let cert = pem.parse_x509().map_err(|e| anyhow!("parse x509: {e}"))?;
    Ok(cert.signature_value.data.to_vec())
}

/// 인증서 PEM 내용(DER)의 SHA-256 지문 (hex). mTLS 페어링 인증서 매칭용.
pub fn cert_fingerprint(cert_pem: &str) -> Result<String> {
    use x509_parser::prelude::*;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|e| anyhow!("parse pem: {e}"))?;
    Ok(hex::encode(Sha256::digest(&pem.contents)))
}

/// SHA-256 헬퍼.
pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// self-signed 인증서 + PKCS#8 개인키 PEM 생성 (RSA-2048, CN=KmcStreamhost, 10년).
/// Sunshine/moonshine와 동일하게 CA·서명/암호화 용도로 생성.
pub fn create_certificate() -> Result<(String, String)> {
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    };
    use std::time::{Duration, SystemTime};

    // RSA-2048 개인키 (순수 rsa 크레이트) → PKCS#8 PEM.
    let mut rng = rand::thread_rng();
    let private_key = RsaPrivateKey::new(&mut rng, 2048).map_err(|e| anyhow!("gen rsa key: {e}"))?;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    let key_pem = private_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| anyhow!("encode pkcs8: {e}"))?
        .to_string();

    let mut params = CertificateParams::default();
    params.not_before = SystemTime::now().into();
    params.not_after = (SystemTime::now() + Duration::from_secs(3650 * 24 * 60 * 60)).into();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "KmcStreamhost");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
        KeyUsagePurpose::KeyAgreement,
    ];

    let key_pair = KeyPair::from_pem(&key_pem).map_err(|e| anyhow!("rcgen keypair from pem: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow!("self-sign cert: {e}"))?;
    Ok((cert.pem(), key_pem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecb_roundtrip() {
        let key = [7u8; 16];
        let data = [0xABu8; 32];
        let ct = aes_ecb_encrypt(&data, &key).unwrap();
        assert_ne!(ct, data);
        let pt = aes_ecb_decrypt(&ct, &key).unwrap();
        assert_eq!(pt, data);
    }

    #[test]
    fn ecb_rejects_unaligned() {
        let key = [0u8; 16];
        assert!(aes_ecb_encrypt(&[0u8; 17], &key).is_err());
    }

    #[test]
    fn gcm_roundtrip_separate_tag() {
        let key = [3u8; 16];
        let iv = [9u8; 12];
        let mut tag = [0u8; 16];
        let pt = b"moonlight control frame";
        let ct = aes_gcm_encrypt(pt, &key, &iv, &mut tag).unwrap();
        assert_ne!(&ct[..], &pt[..]);
        let out = aes_gcm_decrypt(&ct, &key, &iv, &tag).unwrap();
        assert_eq!(out, pt);
        // 태그 변조 시 실패.
        let mut bad = tag;
        bad[0] ^= 1;
        assert!(aes_gcm_decrypt(&ct, &key, &iv, &bad).is_err());
    }

    #[test]
    fn cbc_encrypt_pads() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let ct = aes_cbc_encrypt(b"hello", &key, &iv).unwrap();
        // PKCS7 패딩으로 최소 한 블록.
        assert_eq!(ct.len(), 16);
    }

    #[test]
    fn derive_key_matches_sha256_prefix() {
        let salt = [0u8; 16];
        let key = derive_key(&salt, "1234");
        // 결정적: 동일 입력 동일 출력.
        assert_eq!(key, derive_key(&salt, "1234"));
        assert_ne!(key, derive_key(&salt, "0000"));
    }

    #[test]
    fn cert_gen_and_sign_verify_roundtrip() {
        let (cert_pem, key_pem) = create_certificate().unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        // 자기 인증서 서명값·지문 추출 가능.
        assert!(!cert_signature(&cert_pem).unwrap().is_empty());
        assert_eq!(cert_fingerprint(&cert_pem).unwrap().len(), 64);
        // 개인키로 서명 → 자기 인증서 공개키로 검증.
        let data = b"server_secret payload";
        let sig = rsa_sign_sha256(data, &key_pem).unwrap();
        rsa_verify_cert_sha256(&cert_pem, data, &sig).unwrap();
        // 변조 데이터는 검증 실패.
        assert!(rsa_verify_cert_sha256(&cert_pem, b"tampered", &sig).is_err());
    }
}
