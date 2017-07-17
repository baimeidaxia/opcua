use chrono;

use opcua_types::*;

use crypto::SecurityPolicy;
use crypto::types::*;
use crypto::hash;

use comms::{SecurityHeader, SymmetricSecurityHeader, AsymmetricSecurityHeader, MESSAGE_CHUNK_HEADER_SIZE, SEQUENCE_HEADER_SIZE};
use comms::message_chunk::MessageChunkType;

/// Holds all of the security information related to this session
#[derive(Debug)]
pub struct SecureChannel {
    /// The security mode for the connection, None, Sign, SignAndEncrypt
    pub security_mode: MessageSecurityMode,
    /// The security policy for the connection, None or Encryption/Signing settings
    pub security_policy: SecurityPolicy,
    /// Secure channel id
    pub secure_channel_id: UInt32,
    /// Token creation time.
    pub token_created_at: DateTime,
    /// Token lifetime
    pub token_lifetime: UInt32,
    /// Token identifier
    pub token_id: UInt32,
    /// Our nonce generated while handling open secure channel
    pub nonce: Vec<u8>,
    /// Their nonce provided by open secure channel
    pub their_nonce: Vec<u8>,
    /// Their certificate
    pub their_cert: Option<X509>,
    /// Symmetric Signing Key, Encrypt Key, IV
    pub keys: Option<(Vec<u8>, AesKey, Vec<u8>)>,
    /// Symmetric Signing Key, Decrypt Key, IV
    pub their_keys: Option<(Vec<u8>, AesKey, Vec<u8>)>,
}

impl SecureChannel {
    pub fn new() -> SecureChannel {
        // Invalid secure channel info by default
        SecureChannel {
            security_mode: MessageSecurityMode::None,
            security_policy: SecurityPolicy::None,
            secure_channel_id: 0,
            token_id: 0,
            token_created_at: DateTime::now(),
            token_lifetime: 0,
            nonce: Vec::with_capacity(64),
            their_nonce: Vec::with_capacity(64),
            keys: None,
            their_cert: None,
            their_keys: None,
        }
    }

    pub fn make_security_header(&self, message_type: MessageChunkType) -> SecurityHeader {
        match message_type {
            MessageChunkType::OpenSecureChannel => {
                SecurityHeader::Asymmetric(AsymmetricSecurityHeader::none())
            }
            _ => {
                SecurityHeader::Symmetric(SymmetricSecurityHeader {
                    token_id: self.token_id,
                })
            }
        }
    }

    /// Creates a nonce for the connection. The nonce should be the same size as the symmetric key
    pub fn create_random_nonce(&mut self) {
        if self.signing_enabled() || self.encryption_enabled() {
            use rand::{self, Rng};
            let mut rng = rand::thread_rng();
            self.nonce = vec![0u8; self.security_policy.symmetric_key_size()];
            rng.fill_bytes(&mut self.nonce);
        } else {
            self.nonce = vec![0u8; 1];
        }
    }

    /// Set their nonce which should be the same as the symmetric key
    pub fn set_their_nonce(&mut self, their_nonce: &ByteString) -> Result<(), StatusCode> {
        if let Some(ref their_nonce) = their_nonce.value {
            if (self.signing_enabled() || self.encryption_enabled()) && their_nonce.len() != self.security_policy.symmetric_key_size() {
                Err(BAD_NONCE_INVALID)
            } else {
                self.their_nonce = their_nonce.to_vec();
                Ok(())
            }
        } else {
            Err(BAD_NONCE_INVALID)
        }
    }

    /// Part 6
    /// 6.7.5 
    /// Deriving keys Once the SecureChannel is established the Messages are signed and encrypted with
    /// keys derived from the Nonces exchanged in the OpenSecureChannel call. These keys are derived by passing the Nonces to a pseudo-random function which produces a sequence of bytes from a set of inputs. A pseudo-random function is represented by the following function declaration: 
    ///
    /// ```c++
    /// Byte[] PRF( Byte[] secret,  Byte[] seed,  Int32 length,  Int32 offset)
    /// ```
    ///
    /// Where length is the number of bytes to return and offset is a number of bytes from the beginning of the sequence. 
    ///
    /// The lengths of the keys that need to be generated depend on the SecurityPolicy used for the channel.
    /// The following information is specified by the SecurityPolicy: 
    ///
    /// a) SigningKeyLength (from the DerivedSignatureKeyLength);
    /// b) EncryptingKeyLength (implied by the SymmetricEncryptionAlgorithm);
    /// c) EncryptingBlockSize (implied by the SymmetricEncryptionAlgorithm).
    ///
    /// The parameters passed to the pseudo random function are specified in Table 33. 
    ///
    /// Table 33 – Cryptography key generation parameters 
    ///
    /// Key | Secret | Seed | Length | Offset
    /// ClientSigningKey | ServerNonce | ClientNonce | SigningKeyLength | 0
    /// ClientEncryptingKey | ServerNonce | ClientNonce | EncryptingKeyLength | SigningKeyLength
    /// ClientInitializationVector | ServerNonce | ClientNonce | EncryptingBlockSize | SigningKeyLength + EncryptingKeyLength
    /// ServerSigningKey | ClientNonce | ServerNonce | SigningKeyLength | 0
    /// ServerEncryptingKey | ClientNonce | ServerNonce | EncryptingKeyLength | SigningKeyLength
    /// ServerInitializationVector | ClientNonce | ServerNonce | EncryptingBlockSize | SigningKeyLength + EncryptingKeyLength
    ///  
    /// The Client keys are used to secure Messages sent by the Client. The Server keys
    /// are used to secure Messages sent by the Server.
    /// 
    pub fn derive_keys(&mut self) {
        self.keys = Some(self.security_policy.make_secure_channel_keys(&self.nonce, &self.their_nonce));
        debug!("Derived our keys = {:?}", self.keys);
        self.their_keys = Some(self.security_policy.make_secure_channel_keys(&self.their_nonce, &self.nonce));
        debug!("Derived their keys = {:?}", self.their_keys);
    }

    /// Test if the token has expired yet
    pub fn token_has_expired(&self) -> bool {
        let now = DateTime::now().as_chrono();
        let token_expires = self.token_created_at.as_chrono() + chrono::Duration::seconds(self.token_lifetime as i64);
        if now.ge(&token_expires) { true } else { false }
    }

    pub fn symmetric_signature_size(&self) -> usize {
        if self.security_policy != SecurityPolicy::None {
            self.security_policy.symmetric_signature_size()
        } else {
            0
        }
    }

    /// Calculate the padding size
    ///
    /// Padding adds bytes to the body to make it a multiple of the block size so it can be encrypted.
    pub fn calc_chunk_padding(&self, bytes_to_write: usize, security_header: &SecurityHeader, message_chunk_size: usize) -> usize {
        if self.security_policy != SecurityPolicy::None && self.security_mode != MessageSecurityMode::None {
            // Signature size comes from policy
            let signature_size = self.security_policy.symmetric_signature_size();
            // Plain text block size comes from policy
            let plain_text_block_size = self.security_policy.plain_block_size();

            // If a message chunk size is specified then we need to calculate the max body size
            let max_body_size = if message_chunk_size != 0 {
                // Cipher text block size comes from policy
                let cipher_text_block_size = self.security_policy.cipher_block_size();
                // Header size include message header and security header
                let header_size = MESSAGE_CHUNK_HEADER_SIZE + security_header.byte_len();
                // Sequence header size is 8 bytes
                let sequence_header_size = SEQUENCE_HEADER_SIZE;

                let f1: f64 = (message_chunk_size - header_size - signature_size - 1) as f64;
                let f2: f64 = cipher_text_block_size as f64;
                plain_text_block_size * ((f1 / f2).floor() as usize) - sequence_header_size
            } else {
                0
            };
            let padding_size = if max_body_size > 0 && bytes_to_write > max_body_size {
                0
            }
            else {
                plain_text_block_size - ((bytes_to_write + signature_size + 1) % plain_text_block_size)
            };
            debug!("Padding calculated to be {} bytes", padding_size);
            padding_size
        } else {
            0
        }
    }

    /// Sign the following block
    fn sign(&self, src: &[u8], signature: &mut [u8]) -> Result<(), StatusCode> {
        debug!("Producing signature for {} bytes of data into signature of {} bytes", src.len(), signature.len());
        let key = &(self.keys.as_ref().unwrap()).0;
        match self.security_policy {
            SecurityPolicy::Basic128Rsa15 => {
                // HMAC SHA-1
                hash::hmac_sha1(key, src, signature)
            }
            SecurityPolicy::Basic256 | SecurityPolicy::Basic256Sha256 => {
                // HMAC SHA-256                
                hash::hmac_sha256(key, src, signature)
            }
            _ => {
                panic!("Unsupported policy")
            }
        }
    }

    /// Verify their signature
    fn verify(&self, src: &[u8], signature: &[u8]) -> Result<(), StatusCode> {
        let key = &(self.their_keys.as_ref().unwrap()).0;
        // Verify the signature using SHA-1 / SHA-256 HMAC
        let verified = match self.security_policy {
            SecurityPolicy::Basic128Rsa15 => {
                // HMAC SHA-1
                hash::verify_hmac_sha1(key, src, signature)
            }
            SecurityPolicy::Basic256 | SecurityPolicy::Basic256Sha256 => {
                // HMAC SHA-256                
                hash::verify_hmac_sha256(key, src, signature)
            }
            _ => {
                panic!("Unsupported policy")
            }
        };
        if verified {
            Ok(())
        } else { 
            error!("Signature invalid {:?}", signature);
            Err(BAD_APPLICATION_SIGNATURE_INVALID)
        }
    }

    /// Encrypt the data
    fn encrypt(&self, src: &[u8], dst: &mut [u8]) -> Result<(), StatusCode> {
        let keys = self.keys.as_ref().unwrap();
        let key = &keys.1;
        let iv = &keys.2;
        let result = key.encrypt(src, iv, dst);
        if result.is_ok() {
            Ok(())
        } else {
            error!("Cannot encrypt data, {}", result.unwrap_err());
            Err(BAD_ENCODING_ERROR)
        }
    }

    /// Decrypt the data
    fn decrypt(&self, src: &[u8], dst: &mut [u8]) -> Result<(), StatusCode> {
        let keys = self.their_keys.as_ref().unwrap();
        let key = &keys.1;
        let iv = &keys.2;
        let result = key.decrypt(src, iv, dst);
        if result.is_ok() {
            Ok(())
        } else {
            error!("Cannot decrypt data, {}", result.unwrap_err());
            Err(BAD_DECODING_ERROR)
        }
    }

    // Panic code which requires a policy
    fn expect_supported_security_policy(&self) {
        match self.security_policy {
            SecurityPolicy::Basic128Rsa15 | SecurityPolicy::Basic256 | SecurityPolicy::Basic256Sha256 => {}
            _ => {
                panic!("Unsupported security policy");
            }
        }
    }

    pub fn signing_enabled(&self) -> bool {
        self.security_policy != SecurityPolicy::None && self.security_mode == MessageSecurityMode::Sign
    }

    /// Test if encryption is enabled. 
    pub fn encryption_enabled(&self) -> bool {
        self.security_policy != SecurityPolicy::None && self.security_mode == MessageSecurityMode::SignAndEncrypt
    }

    /// Encode data using security. Destination buffer is expected to be same size as src and expected
    /// to have space for for a signature if a signature is to be appended
    ///
    /// Signing is done first and then encryption
    ///
    /// S - Message Header
    /// S - Security Header
    /// S - Sequence Header - E
    /// S - Body            - E
    /// S - Padding         - E
    ///     Signature       - E
    pub fn encrypt_and_sign(&self, src: &[u8], sign_info: (usize, usize), encrypt_info: (usize, usize), dst: &mut [u8]) -> Result<(), StatusCode> {
        let (s_from, s_to) = sign_info;
        let (e_from, e_to) = encrypt_info;
        match self.security_mode {
            MessageSecurityMode::None => {
                debug!("encrypt_and_sign is doing nothing because security mode == None");
                // Just copy data to out
                dst.copy_from_slice(src);
                Ok(())
            }
            MessageSecurityMode::Sign => {
                debug!("encrypt_and_sign security mode == Sign");
                self.expect_supported_security_policy();
                let signature_len = src.len() - s_to;
                let mut signature = vec![0u8; signature_len];
                debug!("signature len = {}", signature_len);
                // Sign the message header, security header, sequence header, body, padding
                self.sign(&src[s_from..s_to], &mut signature)?;
                &dst[..s_to].copy_from_slice(&src[..s_to]);
                debug!("Signature = {:?}", signature);
                &dst[s_to..].copy_from_slice(&signature);
                Ok(())
            }
            MessageSecurityMode::SignAndEncrypt => {
                debug!("encrypt_and_sign security mode == SignAndEncrypt");
                self.expect_supported_security_policy();

                // There is an expectation that the block is padded so, this is a quick test
                if (e_to - e_from) % 16 != 0 {
                    error!("The plain text block is not padded properly, size = {}", e_to - e_from);
                    return Err(BAD_DECODING_ERROR);
                }

                let mut dst_tmp = vec![0u8; dst.len() + 16]; // tmp includes +16 for blocksize
                let signature_len = src.len() - s_to;
                debug!("signature len = {}", signature_len);
                let mut signature = vec![0u8; signature_len];
                // Sign the message header, security header, sequence header, body, padding
                self.sign(&src[s_from..s_to], &mut signature)?;
                &dst_tmp[..s_to].copy_from_slice(&src[..s_to]);
                &dst_tmp[s_to..].copy_from_slice(&signature);

                // Encrypt the sequence header, payload, signature
                self.encrypt(&dst_tmp[e_from..e_to], &mut dst[e_from..e_to])?;
                // Copy the message header / security header
                &dst[..e_from].copy_from_slice(&dst_tmp[..e_from]);

                Ok(())
            }
            MessageSecurityMode::Invalid => {
                panic!("Message security mode is invalid");
            }
        }
    }

    /// Decrypts and verifies data.
    ///
    /// S - Message Header
    /// S - Security Header
    /// S - Sequence Header - E
    /// S - Body            - E
    /// S - Padding         - E
    ///     Signature       - E
    pub fn decrypt_and_verify(&self, src: &[u8], sign_info: (usize, usize), encrypt_info: (usize, usize), dst: &mut [u8]) -> Result<(), StatusCode> {
        let (s_from, s_to) = sign_info;
        let (e_from, e_to) = encrypt_info;
        match self.security_mode {
            MessageSecurityMode::None => {
                // Copy everything
                let len = src.len();
                &dst[..len].copy_from_slice(&src[..len]);
                Ok(())
            }
            MessageSecurityMode::Sign => {
                self.expect_supported_security_policy();
                // Copy everything
                let len = src.len();
                debug!("copying from slice ..{}", len);
                &dst[..len].copy_from_slice(&src[..len]);
                // Verify signature
                debug!("Verifying range from {}..{} to signature {}..", s_from, s_to, s_to);
                self.verify(&dst[s_from..s_to], &dst[s_to..])?;
                Ok(())
            }
            MessageSecurityMode::SignAndEncrypt => {
                self.expect_supported_security_policy();

                // There is an expectation that the block is padded so, this is a quick test
                if (e_to - e_from) % 16 != 0 {
                    error!("The plain text block is not padded properly, size = {}", e_to - e_from);
                    return Err(BAD_DECODING_ERROR);
                }

                // Copy security header
                &dst[..e_from].copy_from_slice(&src[..e_from]);

                // Decrypt encrypted portion
                let mut decrypted_tmp = vec![0u8; e_to - e_from + 16]; // tmp includes +16 for blocksize
                self.decrypt(&src[e_from..e_to], &mut decrypted_tmp)?;
                &dst[e_from..e_to].copy_from_slice(&decrypted_tmp[..(e_to - e_from)]);

                // Verify signature (after encrypted portion)
                self.verify(&dst[s_from..s_to], &dst[s_to..])?;
                Ok(())
            }
            MessageSecurityMode::Invalid => {
                // Use the security policy to decrypt the block using the token
                panic!("Message security mode is invalid");
            }
        }
    }
}
