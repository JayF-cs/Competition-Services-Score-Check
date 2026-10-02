use std::env;
use tokio::fs;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use serde::Deserialize;
use ironrdp_pdu::x224::X224;
use ironrdp_pdu::nego::{self, ConnectionRequest, ConnectionConfirm, SecurityProtocol, RequestFlags};
use ironrdp_pdu::PduHint;
use ironrdp_core::{encode_buf, WriteBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ironrdp_core::decode;
use native_tls::TlsConnector as NativeTlsConnector;
use tokio_native_tls::{TlsConnector, TlsStream};
use ironrdp_connector::credssp::CredsspSequence;
use ironrdp_connector::{Credentials, ServerName, ConnectorErrorKind};
use ironrdp_connector::sspi::credssp::ClientState;
use ironrdp_connector::sspi::generator::GeneratorState;
use x509_cert::der::Decode as _;

#[derive(Deserialize)]
struct RdpConfig {
    host: String,
    port: Option<u16>,
    domain: Option<String>,
    username: String,
    password: String,
    timeout_secs: Option<u64>,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: rdp <config.json>");
        std::process::exit(1);
    }

    let config_str = fs::read_to_string(&args[1]).await.expect("failed to read config");
    let config: RdpConfig = serde_json::from_str(&config_str).expect("invalid config json");
    let dur = Duration::from_secs(config.timeout_secs.unwrap_or(10));

    let result = timeout(dur, check_rdp(&config)).await;

    match result {
        Ok(Ok(true))  => { println!("ACCESS_GRANTED"); std::process::exit(0); }
        Ok(Ok(false)) => { println!("ACCESS_DENIED");  std::process::exit(1); }
        Ok(Err(e))    => { eprintln!("ERROR: {e}");    std::process::exit(2); }
        Err(_)        => { eprintln!("ERROR: timed out after {:?}", dur); std::process::exit(2); }
    }
}

fn build_x224_connection_request() -> anyhow::Result<Vec<u8>> {
    let request = X224(ConnectionRequest {
        nego_data: Some(nego::NegoRequestData::cookie("user".to_owned())),
        flags: RequestFlags::empty(),
        // Ask for HYBRID_EX too: without it the server will never send an
        // explicit AccessDenied, and you're stuck inferring success/failure
        // purely from post-CredSSP silence vs. a closed/erroring socket.
        protocol: SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX | SecurityProtocol::SSL,
    });

    let mut buf = WriteBuf::new();
    let written = encode_buf(&request, &mut buf)?;
    Ok(buf.filled()[..written].to_vec())
}

fn build_tls_connector() -> anyhow::Result<TlsConnector> {
    let connector = NativeTlsConnector::builder()
        .danger_accept_invalid_certs(true)   // RDP servers self-sign; no CA chain to check
        .danger_accept_invalid_hostnames(true) // you're connecting by IP, not a matching hostname
        .build()?;

    Ok(TlsConnector::from(connector))
}

/// Pulls the DER-encoded SubjectPublicKeyInfo (not the whole cert) out of the
/// peer's TLS certificate. This is what CredSSP's public-key channel binding
/// hashes to defend against relay attacks.
fn extract_server_public_key(tls_stream: &TlsStream<TcpStream>) -> anyhow::Result<Vec<u8>> {
    let cert = tls_stream
        .get_ref()
        .peer_certificate()?
        .ok_or_else(|| anyhow::anyhow!("server presented no TLS certificate"))?;
    let der = cert.to_der()?;

    let parsed = x509_cert::Certificate::from_der(&der)
        .map_err(|e| anyhow::anyhow!("failed to parse server certificate: {e}"))?;

    let spki = parsed
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| anyhow::anyhow!("subject public key BIT STRING is not byte-aligned"))?
        .to_owned();

    Ok(spki)
}

/// Reads exactly the bytes the sequence's PduHint says it needs, growing the
/// buffer one byte at a time until find_size reports a complete length. CredSSP
/// messages are small, so this isn't worth optimizing further for this use case.
async fn read_by_hint(
    stream: &mut TlsStream<TcpStream>,
    hint: &dyn PduHint,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        if let Some((_, len)) = hint
            .find_size(&buf)
            .map_err(|e| anyhow::anyhow!("PduHint::find_size failed: {e}"))?
        {
            if buf.len() < len {
                let mut rest = vec![0u8; len - buf.len()];
                stream.read_exact(&mut rest).await?;
                buf.extend_from_slice(&rest);
            }
            return Ok(buf);
        }
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        buf.push(byte[0]);
    }
}

/// Drives a CredsspProcessGenerator to completion. For NTLM (no Kerberos) this
/// should never actually suspend for a network round-trip — if it does, that's
/// unexpected for this use case and we bail rather than silently hanging.
fn resolve_generator(
    generator: &mut ironrdp_connector::credssp::CredsspProcessGenerator<'_>,
) -> anyhow::Result<ClientState> {
    match generator.start() {
        GeneratorState::Suspended(_request) => {
            anyhow::bail!("CredSSP requested an unexpected network round-trip (Kerberos path?) — not supported here")
        }
        GeneratorState::Completed(result) => {
            result.map_err(|e| anyhow::anyhow!("CredSSP processing error: {e}"))
        }
    }
}

async fn check_rdp(config: &RdpConfig) -> anyhow::Result<bool> {
    let addr = format!("{}:{}", config.host, config.port.unwrap_or(3389));
    let mut stream = TcpStream::connect(&addr).await?;

    let cr_packet = build_x224_connection_request()?;
    stream.write_all(&cr_packet).await?;

    // Get header
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;

    // Get byte length of return packet
    let total_length = u16::from_be_bytes([header[2], header[3]]) as usize;

    let mut body = vec![0u8; total_length - 4];
    stream.read_exact(&mut body).await?;

    let mut full_packet = header.to_vec();
    full_packet.extend_from_slice(&body);

    let X224(confirm) = decode::<X224<ConnectionConfirm>>(&full_packet)?;

    let selected_protocol = match confirm {
        ConnectionConfirm::Response { flags: _, protocol } => {
            if protocol.contains(SecurityProtocol::HYBRID) || protocol.contains(SecurityProtocol::HYBRID_EX) {
                protocol
            } else {
                anyhow::bail!("server did not agree to HYBRID/CredSSP: got {protocol:?}");
            }
        }
        ConnectionConfirm::Failure { code } => {
            anyhow::bail!("server rejected connection: {code:?}");
        }
    };
    if !selected_protocol.contains(SecurityProtocol::HYBRID_EX) {
        eprintln!("warning: server did not agree to HYBRID_EX — no explicit AccessDenied signal available, relying on post-CredSSP silence heuristic");
    }
    eprintln!("X.224 negotiation confirmed HYBRID (ex: {})", selected_protocol.contains(SecurityProtocol::HYBRID_EX));

    let connector = build_tls_connector()?;
    let mut tls_stream = connector.connect(&config.host, stream).await?;
    eprintln!("TLS handshake succeeded");

    let server_public_key = extract_server_public_key(&tls_stream)?;

    let credentials = Credentials::UsernamePassword {
        username: config.username.clone(),
        password: config.password.clone(),
    };
    let domain = config.domain.as_deref();
    let server_name = ServerName::new(config.host.clone());
    let mut buf = WriteBuf::new();

    let (mut sequence, mut ts_request) = CredsspSequence::init(
        credentials,
        domain,
        selected_protocol,
        server_name,
        server_public_key,
        None, // KerberosConfig — None, NTLM only
    )?;

    loop {
        let client_state = {
            let mut generator = sequence.process_ts_request(ts_request);
            resolve_generator(&mut generator)?
        };

        buf.clear();
        let written = sequence.handle_process_result(client_state, &mut buf)?;
        if let Some(len) = written.size() {
            tls_stream.write_all(&buf[..len]).await?;
        }

        let Some(hint) = sequence.next_pdu_hint() else {
            break; // Finished with no HYBRID_EX early-auth step
        };

        let pdu = read_by_hint(&mut tls_stream, hint).await?;

        match sequence.decode_server_message(&pdu) {
            Ok(Some(next)) => ts_request = next,
            Ok(None) => break, // explicit EarlyUserAuthResult::Success
            Err(e) => {
                if matches!(e.kind(), ConnectorErrorKind::AccessDenied) {
                    return Ok(false); // explicit EarlyUserAuthResult::AccessDenied
                }
                return Err(e.into());
            }
        }
    }

    eprintln!("CredSSP sequence completed without an explicit AccessDenied");

    // Fallback heuristic for servers without HYBRID_EX: correct creds should
    // leave the server waiting silently for RDP session setup we never do.
    let mut probe = [0u8; 1];
    match timeout(Duration::from_secs(3), tls_stream.read(&mut probe)).await {
        Err(_) => Ok(true),              // timed out waiting = server went quiet = success
        Ok(Ok(0)) => Ok(false),          // server closed the connection = treat as failure
        Ok(Ok(_)) => Ok(true),           // server sent something else; adjust once you've seen this in testing
        Ok(Err(e)) => Err(e.into()),
    }
}
