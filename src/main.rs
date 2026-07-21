// Thin entry point: forward argv (minus the program name) to the library and use
// its return value as the process exit code. All logic lives in the lib crate so
// tests can drive it directly — this mirrors the Go cmd/vhrn/main.go shape.
fn main() {
    std::process::exit(vhrn::run(std::env::args().skip(1).collect()));
}
