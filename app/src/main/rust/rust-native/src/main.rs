use rust_native::tlsnotary::prove;
fn main() {
    let _ = prove("www.example.com".to_string(), "/".to_string());
}
