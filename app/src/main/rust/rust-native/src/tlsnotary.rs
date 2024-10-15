use futures::{AsyncRead, AsyncWrite};
use http_body_util::Empty;
use hyper::{body::Bytes, Request, StatusCode};
use hyper_util::rt::TokioIo;
use k256::{pkcs8::DecodePrivateKey, SecretKey};
use tlsn_common::config::ProtocolConfig;
use tlsn_common::config::ProtocolConfigValidator;
use tlsn_core::{attestation::AttestationConfig, signing::SignatureAlgId, CryptoProvider};
use tlsn_core::{request::RequestConfig, transcript::TranscriptCommitConfig};
use tlsn_formats::http::{DefaultHttpCommitter, HttpCommit, HttpTranscript};
use tlsn_prover::{Prover, ProverConfig};
use tlsn_verifier::{Verifier, VerifierConfig};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

pub const NOTARY_PRIVATE_KEY: &[u8] = &[1u8; 32];

// Maximum number of bytes that can be sent from prover to server
const MAX_SENT_DATA: usize = 1 << 12;
// Maximum number of bytes that can be received by prover from server
const MAX_RECV_DATA: usize = 1 << 14;

/// Runs a simple Notary with the provided connection to the Prover.
pub async fn run_notary<T: AsyncWrite + AsyncRead + Send + Unpin + 'static>(conn: T) {
    let pem_data = include_str!("notary.key");
    let secret_key = SecretKey::from_pkcs8_pem(pem_data).unwrap().to_bytes();

    let mut provider = CryptoProvider::default();
    provider.signer.set_secp256k1(&secret_key).unwrap();

    // Setup the config. Normally a different ID would be generated
    // for each notarization.
    let config_validator = ProtocolConfigValidator::builder()
        .max_sent_data(MAX_SENT_DATA)
        .max_recv_data(MAX_RECV_DATA)
        .build()
        .unwrap();

    let config = VerifierConfig::builder()
        .protocol_config_validator(config_validator)
        .crypto_provider(provider)
        .build()
        .unwrap();

    let attestation_config = AttestationConfig::builder()
        .supported_signature_algs(vec![SignatureAlgId::SECP256K1])
        .build()
        .unwrap();

    Verifier::new(config)
        .notarize(conn, &attestation_config)
        .await
        .unwrap();
}

// Setting of the application server
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36";

#[tokio::main]
pub async fn prove(domain: String, uri: String) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (prover_socket, notary_socket) = tokio::io::duplex(1 << 16);

    // Start a local simple notary service
    tokio::spawn(run_notary(notary_socket.compat()));
    let domain_rs: &str = &domain;
    // Prover configuration.
    let config = ProverConfig::builder()
        .server_name(domain_rs)
        .protocol_config(
            ProtocolConfig::builder()
                // We must configure the amount of data we expect to exchange beforehand, which will
                // be preprocessed prior to the connection. Reducing these limits will improve
                // performance.
                .max_sent_data(1024)
                .max_recv_data(4096)
                .build()?,
        )
        .build()?;

    // Create a new prover and perform necessary setup.
    let prover = Prover::new(config).setup(prover_socket.compat()).await?;

    // Open a TCP connection to the server.
    let client_socket = tokio::net::TcpStream::connect((domain.clone(), 443)).await?;

    // Bind the prover to the server connection.
    // The returned `mpc_tls_connection` is an MPC TLS connection to the server: all
    // data written to/read from it will be encrypted/decrypted using MPC with
    // the notary.
    let (mpc_tls_connection, prover_fut) = prover.connect(client_socket.compat()).await?;
    let mpc_tls_connection = TokioIo::new(mpc_tls_connection.compat());

    // Spawn the prover task to be run concurrently in the background.
    let prover_task = tokio::spawn(prover_fut);

    // Attach the hyper HTTP client to the connection.
    let (mut request_sender, connection) =
        hyper::client::conn::http1::handshake(mpc_tls_connection).await?;

    // Spawn the HTTP task to be run concurrently in the background.
    tokio::spawn(connection);

    // Build a simple HTTP request with common headers
    let request = Request::builder()
        .uri(uri)
        .header("Host", domain.clone())
        .header("Accept", "*/*")
        // Using "identity" instructs the Server not to use compression for its HTTP response.
        // TLSNotary tooling does not support compression.
        .header("Accept-Encoding", "identity")
        .header("Connection", "close")
        .header("User-Agent", USER_AGENT)
        .body(Empty::<Bytes>::new())?;

    println!("Starting an MPC TLS connection with the server");

    // Send the request to the server and wait for the response.
    let response = request_sender.send_request(request).await?;

    println!("Got a response from the server");

    assert!(response.status() == StatusCode::OK);

    // The prover task should be done now, so we can await it.
    let prover = prover_task.await??;

    // Prepare for notarization.
    let mut prover = prover.start_notarize();

    // Parse the HTTP transcript.
    let transcript = HttpTranscript::parse(prover.transcript())?;

    // Commit to the transcript.
    let mut builder = TranscriptCommitConfig::builder(prover.transcript());

    DefaultHttpCommitter::default().commit_transcript(&mut builder, &transcript)?;

    prover.transcript_commit(builder.build()?);

    // Request an attestation.
    let config = RequestConfig::default();

    let (attestation, secrets) = prover.finalize(&config).await?;

    // Write the attestation to disk.
    tokio::fs::write(
        "example.attestation.tlsn",
        bincode::serialize(&attestation)?,
    )
    .await?;

    // Write the secrets to disk.
    tokio::fs::write("example.secrets.tlsn", bincode::serialize(&secrets)?).await?;

    println!("Notarization completed successfully!");
    println!(
        "The attestation has been written to `example.attestation.tlsn` and the \
        corresponding secrets to `example.secrets.tlsn`."
    );

    Ok(())
}
