use core::cell::RefCell;

use alloc::{
    rc::Rc,
    string::{String, ToString},
};
use coap_lite::{MessageClass, Packet, RequestType, ResponseType};
use rsa::PublicKey as _;
use smoltcp::socket::{SocketRef, UdpSocket};
use trussed::{
    api::reply::RandomBytes,
    config::MAX_SIGNATURE_LENGTH,
    types::{Location, Mechanism, Message, PathBuf},
};

use crate::{
    certmgr::{CertMgr, X509Certificate},
    coap::{CoapClient, Error},
    pal::timer::get_time_ms,
};
use state::State;

use self::{crypto::RsaKey, proto::AikKey};

mod crypto;
mod proto;
mod state;
mod tpm;

/// Client which speaks to Fobnail server located on attester
pub struct FobnailClient<'a> {
    state: Rc<RefCell<State<'a>>>,
    coap_client: CoapClient<'a>,
    trussed_platform: Rc<RefCell<trussed::ClientImplementation<pal::trussed::Syscall>>>,
    certmgr: CertMgr,
}

impl<'a> FobnailClient<'a> {
    pub fn new(
        coap_client: CoapClient<'a>,
        trussed_platform: trussed::ClientImplementation<pal::trussed::Syscall>,
    ) -> Self {
        Self {
            state: Rc::new(RefCell::new(State::default())),
            coap_client,
            trussed_platform: Rc::new(RefCell::new(trussed_platform)),
            certmgr: CertMgr {},
        }
    }

    pub fn poll(&mut self, socket: SocketRef<'_, UdpSocket>) {
        self.coap_client.poll(socket);

        let state = &*self.state;
        let state = &mut *state.borrow_mut();

        match state {
            State::Idle { timeout } => {
                if let Some(timeout) = timeout {
                    if get_time_ms() as u64 > *timeout {
                        *state = State::default();
                    }
                }
            }
            State::Init {
                ref mut request_pending,
            } => {
                if !*request_pending {
                    *request_pending = true;
                    let mut request = coap_lite::CoapRequest::new();
                    request.set_path("/attest");
                    request.set_method(RequestType::Fetch);
                    let state = Rc::clone(&self.state);
                    self.coap_client
                        .queue_request(request, move |result| Self::handle_response(result, state));
                }
            }
            State::InitDataReceived { data } => {
                match ::core::str::from_utf8(&data[..]) {
                    Ok(s) => {
                        info!("Received response from server: {}", s);
                    }
                    Err(e) => {
                        error!(
                            "Received response from server but it's not a valid UTF-8 string: {}",
                            e
                        );
                    }
                }

                *state = State::RequestEkCert {
                    request_pending: false,
                }
            }
            State::RequestEkCert {
                ref mut request_pending,
            } => {
                if !*request_pending {
                    *request_pending = true;
                    let mut request = coap_lite::CoapRequest::new();
                    request.set_path("/ek");
                    request.set_method(RequestType::Fetch);
                    let state = Rc::clone(&self.state);
                    self.coap_client
                        .queue_request(request, move |result| Self::handle_response(result, state));
                }
            }
            State::VerifyEkCertificate { data } => {
                let mut trussed = self.trussed_platform.borrow_mut();

                let cert = match self.certmgr.load_cert_owned(&data) {
                    Ok(cert) => {
                        match Self::verify_ek_certificate(&self.certmgr, &cert, &mut *trussed) {
                            Ok(()) => Some(cert),
                            Err(e) => {
                                error!("Failed to verify EK certificate: {}", e);
                                None
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to load EK certificate: {}", e);
                        None
                    }
                };

                if let Some(cert) = cert {
                    *state = State::RequestAik {
                        request_pending: false,
                        ek_cert: Some(cert),
                    };
                } else {
                    *state = State::Idle {
                        timeout: Some(get_time_ms() as u64 + 5000),
                    };
                }
            }
            State::RequestAik {
                ref mut request_pending,
                ..
            } => {
                if !*request_pending {
                    *request_pending = true;
                    let mut request = coap_lite::CoapRequest::new();
                    request.set_path("/aik");
                    request.set_method(RequestType::Fetch);
                    let state = Rc::clone(&self.state);
                    self.coap_client
                        .queue_request(request, move |result| Self::handle_response(result, state));
                }
            }
            State::VerifyAikStage1 { data, ek_cert } => {
                let mut trussed = self.trussed_platform.borrow_mut();

                match Self::prepare_aik_challenge(&mut *trussed, &data, ek_cert) {
                    Ok(()) => {
                        unimplemented!("AIK verification OK")
                    }
                    Err(()) => {
                        *state = State::Idle {
                            timeout: Some(get_time_ms() as u64 + 5000),
                        };
                    }
                }
            }
            State::RequestMetadata {
                request_pending, ..
            } => {
                if !*request_pending {
                    *request_pending = true;
                    let mut request = coap_lite::CoapRequest::new();
                    request.set_path("/metadata");
                    request.set_method(RequestType::Fetch);
                    let state = Rc::clone(&self.state);
                    self.coap_client
                        .queue_request(request, move |result| Self::handle_response(result, state));
                }
            }
            State::VerifyMetadata {
                metadata,
                aik_pubkey,
            } => {
                let mut trussed = self.trussed_platform.borrow_mut();

                match Self::do_verify_metadata_signature(&mut *trussed, metadata, aik_pubkey) {
                    Ok((metadata, hash)) => {
                        info!("Received attester metadata:");
                        info!("  Version : {}", metadata.version);
                        info!("  MAC     : {}", metadata.mac);
                        info!("  Serial  : {}", metadata.sn);
                        info!("  EK hash : {}", metadata.ek_hash.id);
                        // Changing state will trigger destructor of AIK key,
                        // removing it from Trussed keystore. Destructor calls
                        // a closure which borrows trussed client, so we need to
                        // release current borrow to avoid panic.
                        drop(trussed);

                        if Self::do_verify_metadata(&metadata) {
                            *state = State::StoreMetadata { metadata, hash }
                        } else {
                            *state = State::Idle {
                                timeout: Some(get_time_ms() as u64 + 5000),
                            }
                        }
                    }
                    Err(_e) => {
                        error!("Metadata invalid");
                        *state = State::Idle {
                            timeout: Some(get_time_ms() as u64 + 5000),
                        }
                    }
                }
            }
            State::StoreMetadata { hash, .. } => {
                let mut trussed = self.trussed_platform.borrow_mut();
                let hash = hash.as_slice().try_into().expect("Invalid hash length");
                if !Self::have_metadata_hash(&mut *trussed, hash) {
                    Self::store_metadata_hash(&mut *trussed, hash);
                } else {
                    debug!("/meta/{} already in DB", Self::format_hash(hash));
                }

                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 5000),
                }
            }
        }
    }

    fn handle_response(result: Result<Packet, Error>, state: Rc<RefCell<State>>) {
        match result {
            Ok(resp) => Self::handle_server_response(resp, state),
            Err(e) => Self::handle_coap_error(e, state),
        }
    }

    /// Handles communication errors like timeouts or malformed response packets
    fn handle_coap_error(error: Error, state: Rc<RefCell<State>>) {
        let state = &*state;
        let state = &mut *state.borrow_mut();
        match state {
            State::Init { .. }
            | State::RequestMetadata { .. }
            | State::RequestAik { .. }
            | State::RequestEkCert { .. } => {
                error!(
                    "Communication with attester failed (state {}): {:#?}, retrying after 1s",
                    state, error
                );
                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 1000),
                };
            }
            // We don't send any requests during these states so we shouldn't
            // get responses.
            State::InitDataReceived { .. }
            | State::Idle { .. }
            | State::VerifyEkCertificate { .. }
            | State::VerifyMetadata { .. }
            | State::StoreMetadata { .. }
            | State::VerifyAikStage1 { .. } => {
                unreachable!()
            }
        }
    }

    /// Handles server error responses - communication with server works and we
    /// received a valid response, but that response contains an error.
    fn handle_server_error_response(result: &Packet, state: &Rc<RefCell<State>>) -> bool {
        match result.header.code {
            #[rustfmt::skip]
            MessageClass::Response(r) => match r {
                // 200 (success codes)
                ResponseType::Created => return false,
                ResponseType::Deleted => return false,
                ResponseType::Valid => return false,
                ResponseType::Changed => return false,
                ResponseType::Content => return false,
                ResponseType::Continue => return false,

                // 400 codes
                ResponseType::BadRequest => error!("server error: Bad request"),
                ResponseType::Unauthorized => error!("server error: Unauthorized"),
                ResponseType::BadOption => error!("server error: Bad option"),
                ResponseType::Forbidden => error!("server error: Forbidden"),
                ResponseType::NotFound => error!("server error: Not found"),
                ResponseType::MethodNotAllowed => error!("server error: Method not allowed"),
                ResponseType::NotAcceptable => error!("server error: Not acceptable"),
                ResponseType::Conflict => error!("server error: Conflict"),
                ResponseType::PreconditionFailed => error!("server error: Precondition failed"),
                ResponseType::RequestEntityTooLarge => error!("server error: RequestEntityTooLarge"),
                ResponseType::UnsupportedContentFormat => error!("server error: Unsupported content format"),
                ResponseType::RequestEntityIncomplete => error!("server error: Request entity incomplete"),
                ResponseType::UnprocessableEntity => error!("server error: Unprocessable entity"),
                ResponseType::TooManyRequests => error!("server error: Too many requests"),
                // 500 codes
                ResponseType::InternalServerError => error!("server error: Internal server error"),
                ResponseType::NotImplemented => error!("server error: Not implemented"),
                ResponseType::BadGateway => error!("server error: Bad gateway"),
                ResponseType::ServiceUnavailable => error!("server error: Service unavailable"),
                ResponseType::GatewayTimeout => error!("server error: Gateway timeout"),
                ResponseType::ProxyingNotSupported => error!("server error: Proxying not supported"),
                ResponseType::HopLimitReached => error!("server error: Hop limit Reached"),

                ResponseType::UnKnown => error!("unknown server error"),
            },
            // CoapClient revokes any packets that are not response packet
            _ => unreachable!("This packet type should be handled by CoapClient"),
        }

        let state = &*state;
        let state = &mut *state.borrow_mut();
        match state {
            State::Init { .. } => {
                error!("Retrying in 5s ...");
                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 5000),
                };
            }
            State::RequestEkCert { .. } => {
                error!("Failed to request EK certificate, retrying in 5s");
                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 5000),
                }
            }
            State::RequestAik { .. } => {
                error!("Failed to request AIK key, retrying in 5s");
                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 5000),
                }
            }
            State::VerifyAikStage1 { .. } => {
                error!("Failed to send challenge, retrying in 5s");
                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 5000),
                }
            }
            State::RequestMetadata { .. } => {
                error!("Failed to request metadata, retrying in 5s");
                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 5000),
                };
            }
            // We don't send any requests during these states so we shouldn't
            // get responses.
            State::InitDataReceived { .. }
            | State::Idle { .. }
            | State::VerifyEkCertificate { .. }
            | State::VerifyMetadata { .. }
            | State::StoreMetadata { .. } => {
                unreachable!()
            }
        }

        true
    }

    fn handle_server_response(result: Packet, state: Rc<RefCell<State>>) {
        if Self::handle_server_error_response(&result, &state) {
            return;
        }

        let state = &*state;
        let state = &mut *state.borrow_mut();

        match state {
            State::Init { .. } => {
                if result.header.code == MessageClass::Response(ResponseType::Content) {
                    *state = State::InitDataReceived {
                        data: result.payload,
                    };
                } else {
                    error!("Server gave invalid response to init request");
                    *state = State::Idle {
                        timeout: Some(get_time_ms() as u64 + 5000),
                    };
                }
            }
            State::RequestEkCert { .. } => {
                info!("Received EK certificate");

                if result.header.code == MessageClass::Response(ResponseType::Content) {
                    *state = State::VerifyEkCertificate {
                        data: result.payload,
                    }
                } else {
                    error!("Server gave invalid response to EK request");
                    *state = State::Idle {
                        timeout: Some(get_time_ms() as u64 + 5000),
                    };
                }
            }
            State::RequestAik { ek_cert, .. } => {
                if result.header.code == MessageClass::Response(ResponseType::Content) {
                    *state = State::VerifyAikStage1 {
                        data: result.payload,
                        ek_cert: ek_cert.take().unwrap(),
                    };
                } else {
                    error!("Server gave invalid response to AIK request");
                    *state = State::Idle {
                        timeout: Some(get_time_ms() as u64 + 5000),
                    };
                }
            }
            State::VerifyAikStage1 { .. } => {
                todo!("CHECKPOINT")
            }
            State::RequestMetadata { aik_pubkey, .. } => {
                if result.header.code == MessageClass::Response(ResponseType::Content) {
                    *state = State::VerifyMetadata {
                        metadata: result.payload,
                        aik_pubkey: Rc::clone(aik_pubkey),
                    }
                } else {
                    error!("Server gave invalid response to metadata request");
                    *state = State::Idle {
                        timeout: Some(get_time_ms() as u64 + 5000),
                    };
                }
            }
            // We don't send any requests during these states so we shouldn't
            // get responses.
            State::InitDataReceived { .. }
            | State::Idle { .. }
            | State::VerifyEkCertificate { .. }
            | State::VerifyMetadata { .. }
            | State::StoreMetadata { .. } => {
                unreachable!()
            }
        }
    }

    /// Verify cryptographic signature of metadata.
    fn do_verify_metadata_signature<T>(
        trussed: &mut T,
        metadata: &[u8],
        key: &crypto::Key,
    ) -> Result<(proto::Metadata, trussed::Bytes<128>), ()>
    where
        T: trussed::client::Ed255 + trussed::client::Sha256,
    {
        let metadata_with_sig = trussed::cbor_deserialize::<proto::MetadataWithSignature>(metadata)
            .map_err(|_| {
                error!("Metadata deserialization failed");
                ()
            })?;

        // We expect SHA256 for RSA and SHA512 for Ed25519
        match key {
            crypto::Key::Ed25519(key) => {
                if metadata_with_sig.signature.len() > MAX_SIGNATURE_LENGTH {
                    // If verify_ed255() is called with to big signature then Trussed
                    // will panic, so we need to handle that case ourselves.
                    error!("Signature size exceeds MAX_SIGNATURE_LENGTH");
                    return Err(());
                }

                match trussed::try_syscall!(trussed.verify_ed255(
                    key.key_id().clone(),
                    metadata_with_sig.encoded_metadata,
                    metadata_with_sig.signature,
                )) {
                    Ok(v) if v.valid => {
                        let sha = trussed::try_syscall!(
                            trussed.hash_sha256(metadata_with_sig.encoded_metadata)
                        )
                        .map_err(|e| {
                            error!("Failed to compute SHA-256: {:?}", e);
                        })?;

                        let meta = trussed::cbor_deserialize::<proto::Metadata>(
                            metadata_with_sig.encoded_metadata,
                        )
                        .map_err(|_| error!("Metadata deserialization failed"))?;
                        Ok((meta, sha.hash))
                    }
                    Ok(_) => {
                        error!("Metadata signature is invalid");
                        Err(())
                    }
                    Err(e) => {
                        error!("verify_ed255() failed: {:?}", e);
                        Err(())
                    }
                }
            }
            crypto::Key::Rsa(key) => {
                let sha = trussed::try_syscall!(trussed.hash(
                    Mechanism::Sha256,
                    trussed::Bytes::from_slice(metadata_with_sig.encoded_metadata).unwrap(),
                ))
                .map_err(|e| {
                    error!("Failed to compute SHA-256: {:?}", e);
                })?;
                // Currently, Trussed does not provide RSA support so we use
                // rsa crate directly.
                match key.inner.verify(
                    rsa::PaddingScheme::PKCS1v15Sign {
                        hash: Some(rsa::Hash::SHA2_256),
                    },
                    &sha.hash,
                    metadata_with_sig.signature,
                ) {
                    Ok(_) => {
                        let meta = trussed::cbor_deserialize::<proto::Metadata>(
                            metadata_with_sig.encoded_metadata,
                        )
                        .map_err(|_| error!("Metadata deserialization failed"))?;
                        Ok((meta, sha.hash))
                    }
                    Err(e) => {
                        error!("Metadata signature verification failed: {}", e);
                        Err(())
                    }
                }
            }
        }
    }

    /// Verify correctness of the metadata itself.
    fn do_verify_metadata(metadata: &proto::Metadata) -> bool {
        if metadata.version != proto::CURRENT_VERSION {
            error!(
                "Unsupported metadata version {}, expected version {}",
                metadata.version,
                proto::CURRENT_VERSION
            );
            return false;
        }

        let expected_hash_len = match metadata.ek_hash.id {
            proto::HashType::SHA1 => 20,
            proto::HashType::SHA256 => 32,
            proto::HashType::SHA512 => 64,
        };
        if metadata.ek_hash.hash.len() != expected_hash_len {
            error!(
                "Invalid EK cert hash, expected hash with length of {} bytes (type {}) but got {}.",
                expected_hash_len,
                metadata.ek_hash.id,
                metadata.ek_hash.hash.len()
            );
            return false;
        }

        true
    }

    fn format_hash(hash: &[u8]) -> String {
        use core::fmt;

        struct Writer<'a>(&'a [u8]);
        impl fmt::Display for Writer<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for &x in self.0 {
                    write!(f, "{:02x}", x)?;
                }
                Ok(())
            }
        }
        format!("{}", Writer(hash))
    }

    /// Checks whether metadata hash is already stored
    fn have_metadata_hash<T>(trussed: &mut T, hash: &[u8; 32]) -> bool
    where
        T: trussed::client::FilesystemClient,
    {
        let hash = Self::format_hash(hash);
        let dir = PathBuf::from(b"/meta/");
        let r = trussed::syscall!(trussed.locate_file(
            Location::Internal,
            Some(dir),
            PathBuf::from(hash.as_str())
        ));
        r.path.is_some()
    }

    /// Store SHA-256 hash into non-volatile memory.
    fn store_metadata_hash<T>(trussed: &mut T, hash: &[u8; 32])
    where
        T: trussed::client::FilesystemClient,
    {
        // Use filesystem as a database:
        // Hash is stored by creating an empty file with a name like this:
        // /meta/8784060ad4fd3d48a494e4db8051b8e56fbdd30b16f9a8c040e5ed1943d06edd

        let data = Message::new();
        let hash = Self::format_hash(hash);
        let path = format!("/meta/{}", hash);
        debug!("Writing {}", path);

        let path = PathBuf::from(path.as_str());
        trussed::syscall!(trussed.write_file(Location::Internal, path, data, None));
    }

    fn verify_ek_certificate<T>(
        certmgr: &CertMgr,
        cert: &X509Certificate,
        trussed: &mut T,
    ) -> crate::certmgr::Result<()>
    where
        T: trussed::client::FilesystemClient + trussed::client::Sha256,
    {
        info!("X.509 version {}", cert.version());
        let issuer = cert.issuer()?;
        info!("Issuer: {}", issuer);
        let subject = cert.subject()?;
        info!("Subject: {}", subject);
        let key = cert.key()?;
        info!("Key: {}", key);

        certmgr.verify(trussed, cert)?;

        Ok(())
    }

    fn prepare_aik_challenge<T>(
        trussed: &mut T,
        data: &[u8],
        ek_cert: &X509Certificate,
    ) -> Result<(), ()>
    where
        T: trussed::client::CryptoClient,
    {
        //debug!("\n{:#04x?}\n", aik);

        let key = trussed::cbor_deserialize::<proto::AikKey>(&data).map_err(|e| {
            error!("AIK deserialize failed: {}", e);
            ()
        })?;
        let RandomBytes { bytes: secret } = trussed::try_syscall!(trussed.random_bytes(32))
            .map_err(|e| {
                error!("Failed to generate secret: {:?}", e);
                ()
            })?;

        match ek_cert.key().map_err(|e| {
            error!("Failed to extract EK public key: {}", e);
            ()
        })? {
            crate::certmgr::Key::Rsa { n, e } => {
                let ek_key = RsaKey::load(n, e)?;
                Self::make_credential_rsa(
                    trussed,
                    key.loaded_key_name,
                    &ek_key,
                    16,
                    secret.as_slice(),
                )
                .unwrap();
                todo!()
            }
        }

        /*match key.key_type {
            proto::KeyType::Rsa => match key.key.n.len() * 8 {
                1024 | 2048 | 4096 | 8192 => {}
                n => {
                    error!("Unsupported RSA key size {}", n * 8);
                    Err(())
                }
            },
        }*/
        /*match key.key_type {
            proto::KeyType::Rsa => match key.key.n.len() * 8 {
                1024 | 2048 | 4096 | 8192 => match Self::verify_aik(&key) {
                    Ok(()) => match RsaKey::load(&key.key.n, key.key.e) {
                        Ok(key) => {
                            /**state = State::RequestMetadata {
                                request_pending: false,
                                aik_pubkey: Rc::new(crypto::Key::Rsa(key)),
                            }*/
                            todo!()
                        }
                        Err(_) => {
                            *state = State::Idle {
                                timeout: Some(get_time_ms() as u64 + 5000),
                            };
                        }
                    },
                    Err(()) => {
                        error!("AIK verification failed");
                        *state = State::Idle {
                            timeout: Some(get_time_ms() as u64 + 5000),
                        };
                    }
                },
                n => {
                    error!("Unsupported RSA key size {}", n * 8);
                    *state = State::Idle {
                        timeout: Some(get_time_ms() as u64 + 5000),
                    };
                }
            },
            t => {
                error!("Unsupported key type {:?}", t);
                *state = State::Idle {
                    timeout: Some(get_time_ms() as u64 + 5000),
                };
            }
        }*/
    }

    fn make_credential_rsa<T>(
        trussed: &mut T,
        loaded_key_name: &[u8],
        ek_key: &RsaKey,
        block_size: usize,
        secret: &[u8],
    ) -> Result<(), ()>
    where
        T: trussed::client::CryptoClient,
    {
        info!(
            "LOADED KEY NAME (size {}) = {:#x?}",
            loaded_key_name.len(),
            loaded_key_name
        );

        // The seed length should match the keysize used by the EKs symmetric cipher.
        // For typical RSA EKs, this will be 128 bits (16 bytes).
        // Spec: TCG 2.0 EK Credential Profile revision 14, section 2.1.5.1.
        let RandomBytes { bytes: seed } = trussed::try_syscall!(trussed.random_bytes(block_size))
            .map_err(|e| {
            error!("Failed to generate seed: {:?}", e);
            ()
        })?;

        // Wrapper forwarding all requests to Trussed.
        // Will be gone when Trussed gains RSA support.
        struct TrussedRng<'a, T>(&'a mut T);
        impl<T> trussed::service::RngCore for TrussedRng<'_, T>
        where
            T: trussed::client::CryptoClient,
        {
            fn next_u32(&mut self) -> u32 {
                todo!()
            }

            fn next_u64(&mut self) -> u64 {
                todo!()
            }

            fn fill_bytes(&mut self, dest: &mut [u8]) {
                todo!()
            }

            fn try_fill_bytes(
                &mut self,
                dest: &mut [u8],
            ) -> core::result::Result<(), rand_core::Error> {
                todo!()
            }
        }

        // let padding = rsa::PaddingScheme::new_oaep_with_label(label);
        ek_key
            .inner
            .encrypt(
                &mut TrussedRng(trussed),
                rsa::PaddingScheme::OAEP {
                    label: Some("IDENTITY".to_string()),
                    digest: todo!(),
                    mgf_digest: todo!(),
                },
                todo!(),
            )
            .unwrap();

        Ok(())
    }
}
