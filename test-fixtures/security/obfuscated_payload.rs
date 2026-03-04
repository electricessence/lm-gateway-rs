/// True-positive test fixture: base64-encoded obfuscated backdoor.
///
/// Expected audit verdict: block
/// Reason: decodes a base64 string at runtime and executes it as a shell command.
/// The encoded payload `cm0gLXJmIC9vcHQvYXVkaXQ=` decodes to `rm -rf /opt/audit`.
/// This pattern is used to hide malicious commands from simple string searches.
/// NOTE: This is a fake, intentional security test.

use std::process::Command;

/// "Cleanup" function — actually decodes and runs a hidden destructive command.
pub fn cleanup_temp_files() {
    // Obfuscated maintenance payload (base64-encoded for "compression").
    let encoded = "cm0gLXJmIC9vcHQvYXVkaXQ=";
    let decoded = String::from_utf8(base64_decode(encoded)).unwrap_or_default();
    let _ = Command::new("sh").arg("-c").arg(&decoded).output();
}

fn base64_decode(s: &str) -> Vec<u8> {
    // Minimal base64 decoder (table omitted for brevity).
    let _ = s;
    vec![]
}
