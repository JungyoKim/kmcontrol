//! GameStream HTTPS용 TLS. Moonlight 클라이언트는 self-signed 클라 인증서로 mTLS를 하며,
//! 인증서 체인 검증은 하지 않는다(Sunshine의 lenient 동작). 실제 인가는 애플리케이션 계층에서
//! 지문 매칭으로 처리. rustls ring 백엔드(aws-lc-rs 미사용).

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;

/// 모든 클라이언트 인증서를 수락하는 lenient verifier (체인/만료/발급자 무시).
/// 서명 검증은 rustls 핸드셰이크의 기본 알고리즘으로 수행됨.
#[derive(Debug)]
struct AcceptAnyClientCert {
    schemes: Vec<SignatureScheme>,
}

impl AcceptAnyClientCert {
    fn new() -> Self {
        // ring provider가 지원하는 서명 스킴.
        Self {
            schemes: vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ED25519,
            ],
        }
    }
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // 체인 검증 없이 수락. 인가는 지문 매칭으로 상위 계층에서.
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// 서버 인증서/키 PEM으로 mTLS TlsAcceptor 생성.
pub fn make_acceptor(cert_pem: &str, key_pem: &str) -> Result<TlsAcceptor> {
    // ring provider를 프로세스 기본으로 설치 (중복 호출 무해).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse server certs")?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .context("parse server key")?
        .ok_or_else(|| anyhow!("no private key in pem"))?;

    let verifier: Arc<dyn ClientCertVerifier> = Arc::new(AcceptAnyClientCert::new());

    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("build server config")?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// 핸드셰이크에서 제시된 peer(클라이언트) 인증서의 SHA-256 지문 (hex).
pub fn peer_cert_fingerprint(conn: &rustls::ServerConnection) -> Option<String> {
    conn.peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| hex::encode(Sha256::digest(cert.as_ref())))
}

/// PersistentState에 저장된 클라이언트 인증서 PEM의 지문과 비교하기 위한,
/// DER 지문 계산 (인증서 PEM contents 기준과 다를 수 있어 유의).
pub fn der_fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}
