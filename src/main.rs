// Thin entry point: forward argv (minus the program name) to the library and use
// its return value as the process exit code. All logic lives in the lib crate so
// tests can drive it directly.
fn main() {
    // Install the tracing subscriber before dispatch so diagnostics are captured.
    vhrn::init_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(vhrn::run(&args));
}
