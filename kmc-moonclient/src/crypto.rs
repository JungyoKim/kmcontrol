//! 클라이언트측 페어링 암호 프리미티브 (호스트 crypto.rs의 대칭 구현).
//!
//! 페어링 챌린지: AES-128-ECB(패딩 없음), 키 유도 SHA-256(salt||pin)[..16],
//! 서명 RSA PKCS#1 v1.5 + SHA-256. moonlight-qt nvpairingmanager.cpp와 동일 연산.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, bail, Result};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::sha2::Sha256 as RsaSha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::RsaPrivateKey;
use sha2::{Digest, Sha256};

/// SHA-256(salt || pin)의 앞 16바이트를 AES 키로.
pub fn derive_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(pin.as_bytes());
    let hash = h.finalize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&hash[..16]);
    key
}

/// AES-128-ECB 암호화 (블록별, 패딩 없음).
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

/// RSA PKCS#1 v1.5 + SHA-256 서명. `key_pem`은 PKCS#8 PEM.
pub fn rsa_sign_sha256(data: &[u8], key_pem: &str) -> Result<Vec<u8>> {
    let pk = RsaPrivateKey::from_pkcs8_pem(key_pem).map_err(|e| anyhow!("parse private key: {e}"))?;
    let signing = SigningKey::<RsaSha256>::new(pk);
    Ok(signing.sign(data).to_vec())
}

/// 서버 인증서(PEM) 공개키로 RSA PKCS#1 v1.5 + SHA-256 서명 검증 (MITM 방지).
pub fn rsa_verify_cert_sha256(cert_pem: &str, data: &[u8], signature: &[u8]) -> Result<()> {
    use x509_parser::prelude::*;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|e| anyhow!("parse server pem: {e}"))?;
    let cert = pem.parse_x509().map_err(|e| anyhow!("parse server x509: {e}"))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let public_key =
        rsa::RsaPublicKey::from_public_key_der(spki_der).map_err(|e| anyhow!("extract pubkey: {e}"))?;
    let verifying = VerifyingKey::<RsaSha256>::new(public_key);
    let sig = Signature::try_from(signature).map_err(|e| anyhow!("parse signature: {e}"))?;
    verifying
        .verify(data, &sig)
        .map_err(|e| anyhow!("server signature verify failed: {e}"))
}

/// X.509 인증서 PEM의 signatureValue 비트스트링 추출 (챌린지 해시 재료).
pub fn cert_signature(cert_pem: &str) -> Result<Vec<u8>> {
    use x509_parser::prelude::*;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|e| anyhow!("parse pem: {e}"))?;
    let cert = pem.parse_x509().map_err(|e| anyhow!("parse x509: {e}"))?;
    Ok(cert.signature_value.data.to_vec())
}

/// 인증서 PEM DER의 SHA-256 지문 (hex).
pub fn cert_fingerprint(cert_pem: &str) -> Result<String> {
    use x509_parser::prelude::*;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|e| anyhow!("parse pem: {e}"))?;
    Ok(hex::encode(Sha256::digest(&pem.contents)))
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// self-signed RSA-2048 클라이언트 인증서 + PKCS#8 개인키 PEM 생성.
/// CN='NVIDIA GameStream Client' (moonlight-qt identitymanager.cpp와 동일).
/// RSA 키는 rsa 크레이트로 생성해 rcgen에 PEM으로 전달 (ring 백엔드는 RSA 생성 불가).
pub fn create_client_certificate() -> Result<(String, String)> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};

    let mut rng = rand::thread_rng();
    let private_key = RsaPrivateKey::new(&mut rng, 2048).map_err(|e| anyhow!("gen rsa key: {e}"))?;
    let key_pem = private_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| anyhow!("encode pkcs8: {e}"))?
        .to_string();

    let mut params = CertificateParams::new(vec![]).map_err(|e| anyhow!("cert params: {e}"))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "NVIDIA GameStream Client");
    params.distinguished_name = dn;

    let key_pair = KeyPair::from_pem(&key_pem).map_err(|e| anyhow!("rcgen keypair from pem: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow!("self-sign: {e}"))?;
    Ok((cert.pem(), key_pem))
}
