use rustls::internal::msgs::{base::PayloadU16, codec::Codec};
use rustls::internal::msgs::quic::Parameter;
use rustls::internal::msgs::quic::{ClientTransportParameters, ServerTransportParameters};
use rustls::{ClientConfig, NoClientAuth, ProtocolVersion};
use rustls::quic::{ClientSession, QuicSecret, ServerSession, TLSResult};

use std::sync::Arc;

use crypto::Secret;
use types::{DRAFT_10, TransportParameter};

use webpki::{DNSNameRef, TLSServerTrustAnchors};
use webpki_roots;

pub use rustls::{Certificate, PrivateKey, ServerConfig, SupportedCipherSuite, TLSError};

pub struct ClientTls {
    pub session: ClientSession,
}

impl ClientTls {
    pub fn new() -> Self {
        Self::with_config(Self::build_config(None))
    }

    pub fn with_config(config: ClientConfig) -> Self {
        Self {
            session: ClientSession::new(&Arc::new(config)),
        }
    }

    pub fn build_config(anchors: Option<&TLSServerTrustAnchors>) -> ClientConfig {
        let mut config = ClientConfig::new();
        let anchors = anchors.unwrap_or(&webpki_roots::TLS_SERVER_ROOTS);
        config.root_store.add_server_trust_anchors(anchors);
        config.versions = vec![ProtocolVersion::TLSv1_3];
        config.alpn_protocols = vec![ALPN_PROTOCOL.into()];
        config
    }

    pub fn get_handshake(&mut self, hostname: &str) -> Result<(Vec<u8>, Option<Secret>), TLSError> {
        let pki_server_name = DNSNameRef::try_from_ascii_str(hostname).unwrap();
        let params = ClientTransportParameters {
            initial_version: 1,
            parameters: encode_transport_parameters(&vec![
                TransportParameter::InitialMaxStreamData(131072),
                TransportParameter::InitialMaxData(1048576),
                TransportParameter::IdleTimeout(300),
            ]),
        };
        Ok(process_tls_result(self.session.get_handshake(pki_server_name, params)?))
    }
}

impl QuicTls for ClientTls {
    fn process_handshake_messages(
        &mut self,
        input: &[u8],
    ) -> Result<(Vec<u8>, Option<Secret>), TLSError> {
        Ok(process_tls_result(self.session.process_handshake_messages(input)?))
    }
}

pub struct ServerTls {
    session: ServerSession,
}

impl ServerTls {
    pub fn with_config(config: &Arc<ServerConfig>) -> Self {
        Self {
            session: ServerSession::new(
                config,
                ServerTransportParameters {
                    negotiated_version: DRAFT_10,
                    supported_versions: vec![DRAFT_10],
                    parameters: encode_transport_parameters(&vec![
                        TransportParameter::InitialMaxStreamData(131072),
                        TransportParameter::InitialMaxData(1048576),
                        TransportParameter::IdleTimeout(300),
                    ]),
                },
            ),
        }
    }

    pub fn build_config(cert_chain: Vec<Certificate>, key: PrivateKey) -> ServerConfig {
        let mut config = ServerConfig::new(NoClientAuth::new());
        config.set_protocols(&[ALPN_PROTOCOL.into()]);
        config.set_single_cert(cert_chain, key);
        config
    }
}

impl QuicTls for ServerTls {
    fn process_handshake_messages(
        &mut self,
        input: &[u8],
    ) -> Result<(Vec<u8>, Option<Secret>), TLSError> {
        Ok(process_tls_result(self.session.get_handshake(input)?))
    }
}

fn process_tls_result(res: TLSResult) -> (Vec<u8>, Option<Secret>) {
    let TLSResult {
        messages,
        key_ready,
    } = res;
    let secret = if let Some((suite, QuicSecret::For1RTT(secret))) = key_ready {
        let (aead_alg, hash_alg) = (suite.get_aead_alg(), suite.get_hash());
        Some(Secret::For1Rtt(aead_alg, hash_alg, secret))
    } else {
        None
    };
    (messages, secret)
}

pub trait QuicTls {
    fn process_handshake_messages(
        &mut self,
        input: &[u8],
    ) -> Result<(Vec<u8>, Option<Secret>), TLSError>;
}

pub fn encode_transport_parameters(params: &[TransportParameter]) -> Vec<Parameter> {
    use self::TransportParameter::*;
    let mut ret = Vec::new();
    for param in params {
        let mut bytes = Vec::new();
        match *param {
            InitialMaxStreamData(v)
            | InitialMaxData(v)
            | InitialMaxStreamIdBidi(v)
            | InitialMaxStreamIdUni(v) => {
                v.encode(&mut bytes);
            }
            IdleTimeout(v) | MaxPacketSize(v) => {
                v.encode(&mut bytes);
            }
            OmitConnectionId => {}
            StatelessResetToken(ref v) => {
                bytes.extend_from_slice(&v);
            }
            AckDelayExponent(v) => {
                v.encode(&mut bytes);
            }
        }
        ret.push((tag(param), PayloadU16::new(bytes)));
    }
    ret
}

fn tag(param: &TransportParameter) -> u16 {
    use self::TransportParameter::*;
    match *param {
        InitialMaxStreamData(_) => 0,
        InitialMaxData(_) => 1,
        InitialMaxStreamIdBidi(_) => 2,
        IdleTimeout(_) => 3,
        OmitConnectionId => 4,
        MaxPacketSize(_) => 5,
        StatelessResetToken(_) => 6,
        AckDelayExponent(_) => 7,
        InitialMaxStreamIdUni(_) => 8,
    }
}

const ALPN_PROTOCOL: &'static str = "hq-10";