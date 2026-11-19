use qrcode::{Color, EcLevel, QrCode};

/// Quiet zone around the code, in modules (the QR spec asks for 4).
pub const QUIET: usize = 4;

/// Payload Android's "Pair device with QR code" scanner expects.
pub fn pairing_payload(service_name: &str, password: &str) -> String {
    format!("WIFI:T:ADB;S:{service_name};P:{password};;")
}

/// A QR code as a square grid of modules, without the quiet zone.
#[derive(Clone)]
pub struct Matrix {
    pub width: usize,
    dark: Vec<bool>,
}

impl Matrix {
    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        self.dark[y * self.width + x]
    }
}

pub fn encode(payload: &str) -> Result<Matrix, String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)
        .map_err(|e| format!("could not build QR code: {e}"))?;
    let dark = code.to_colors().into_iter().map(|c| c == Color::Dark).collect();
    Ok(Matrix { width: code.width(), dark })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_format() {
        assert_eq!(pairing_payload("adbqr-abc", "pw123"), "WIFI:T:ADB;S:adbqr-abc;P:pw123;;");
    }

    #[test]
    fn encode_dimensions() {
        let m = encode(&pairing_payload("adbqr-abcdefgh", "0123456789")).unwrap();
        assert!(m.width >= 21);
        // Finder pattern: the top-left corner module is always dark.
        assert!(m.is_dark(0, 0));
        assert!(!m.is_dark(7, 0));
    }
}
