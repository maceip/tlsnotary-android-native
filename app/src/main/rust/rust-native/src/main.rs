use rust_native::tlsnotary::prove;
fn main() {
    let _ = prove(
        "notary.codes".to_string(),
        "www.example.com".to_string(),
        "/".to_string(),
    );
}
