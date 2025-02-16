use aes::cipher::BlockEncryptMut;
use aes::cipher::KeyIvInit;
use log::info;
use prost::Message;
use rand::Rng;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::signature::Verifier;
use thiserror::Error;

use crate::ffi::cdm;
use crate::video_widevine;
use crate::CdmError;

const ROOT_PUBKEY: &[u8] = include_bytes!("service_certificate_root.der");

/// This is like [`video_widevine::DrmDeviceCertificate`] but with no optional
/// fields, for infallible Client ID enccryption.
pub struct ServerCertificate {
    key: rsa::RsaPublicKey,
    serial_number: Vec<u8>,
    provider_id: String,
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ServerCertificateError {
    #[error("certificate not present")]
    CertificateEmpty,
    #[error("bad protobuf serialization")]
    BadProto(#[from] prost::DecodeError),
    #[error("missing protobuf fields")]
    MissingFields,
    #[error("could not verify signature")]
    BadSignature(#[from] rsa::signature::Error),
    #[error("couldn't parse certificate public key")]
    MalformedKey(#[from] rsa::pkcs1::Error),
    #[error("wrong certificate type {0}")]
    WrongType(i32),
}

impl CdmError for ServerCertificateError {
    fn cdm_exception(&self) -> cdm::Exception {
        match self {
            Self::CertificateEmpty => cdm::Exception::kExceptionTypeError,
            _ => cdm::Exception::kExceptionInvalidStateError,
        }
    }
}

pub fn parse_service_cert_message(
    message: &[u8],
) -> Result<ServerCertificate, ServerCertificateError> {
    let response = video_widevine::SignedMessage::decode(message)?;

    if response.r#type
        != Some(video_widevine::signed_message::MessageType::ServiceCertificate as i32)
    {
        // TODO: Maybe a different type here?
        return Err(ServerCertificateError::CertificateEmpty);
    }

    parse_service_certificate(Some(response.msg()))
}

pub fn parse_service_certificate(
    server_certificate: Option<&[u8]>,
) -> Result<ServerCertificate, ServerCertificateError> {
    let signed_cert_bytes = match server_certificate {
        None | Some(&[]) => return Err(ServerCertificateError::CertificateEmpty),
        Some(v) => v,
    };

    let signed_cert = video_widevine::SignedDrmDeviceCertificate::decode(signed_cert_bytes)?;

    let cert_bytes = signed_cert
        .drm_certificate
        .ok_or(ServerCertificateError::MissingFields)?;

    let signature = rsa::pss::Signature::try_from(
        signed_cert
            .signature
            .ok_or(ServerCertificateError::MissingFields)?
            .as_slice(),
    )?;

    let service_key = rsa::RsaPublicKey::from_pkcs1_der(ROOT_PUBKEY).unwrap();
    let verifying_key = rsa::pss::VerifyingKey::<sha1::Sha1>::new(service_key);
    verifying_key.verify(&cert_bytes, &signature)?;

    let cert = video_widevine::DrmDeviceCertificate::decode(cert_bytes.as_slice())?;

    let cert_type = cert.r#type.ok_or(ServerCertificateError::MissingFields)?;
    if cert_type != video_widevine::drm_device_certificate::CertificateType::Service as i32 {
        return Err(ServerCertificateError::WrongType(cert_type));
    }

    let res = ServerCertificate {
        key: rsa::RsaPublicKey::from_pkcs1_der(cert.public_key())?,
        serial_number: cert
            .serial_number
            .ok_or(ServerCertificateError::MissingFields)?,
        provider_id: cert
            .provider_id
            .ok_or(ServerCertificateError::MissingFields)?,
    };

    info!("Service certificate provider: {}", res.provider_id);

    Ok(res)
}

pub fn encrypt_client_id(
    cert: &ServerCertificate,
    client_id: &video_widevine::ClientIdentification,
) -> video_widevine::EncryptedClientIdentification {
    let mut rng = rand::rng();
    let privacy_key: Vec<u8> = (0..16).map(|_| rng.random()).collect();
    let privacy_iv: Vec<u8> = (0..16).map(|_| rng.random()).collect();

    let client_id_encryptor =
        cbc::Encryptor::<aes::Aes128>::new_from_slices(&privacy_key, &privacy_iv).unwrap();

    let encrypted_client_id = client_id_encryptor
        .encrypt_padded_vec_mut::<aes::cipher::block_padding::Pkcs7>(
            client_id.encode_to_vec().as_slice(),
        );

    let rsa_padding = rsa::Oaep::new::<sha1::Sha1>();
    let encrypted_privacy_key = cert
        .key
        .encrypt(&mut rand8::thread_rng(), rsa_padding, &privacy_key)
        .unwrap();

    video_widevine::EncryptedClientIdentification {
        provider_id: Some(cert.provider_id.clone()),
        service_certificate_serial_number: Some(cert.serial_number.clone()),
        encrypted_client_id: Some(encrypted_client_id),
        encrypted_client_id_iv: Some(privacy_iv),
        encrypted_privacy_key: Some(encrypted_privacy_key),
    }
}
