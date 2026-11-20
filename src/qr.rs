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

/// The code as an SVG image with a white quiet zone, one unit per module.
pub fn to_svg(m: &Matrix) -> String {
    let side = m.width + 2 * QUIET;
    let mut path = String::new();
    for y in 0..m.width {
        let mut x = 0;
        while x < m.width {
            if !m.is_dark(x, y) {
                x += 1;
                continue;
            }
            // One rectangle per horizontal run of dark modules.
            let start = x;
            while x < m.width && m.is_dark(x, y) {
                x += 1;
            }
            path.push_str(&format!("M{} {}h{}v1h-{}z", start + QUIET, y + QUIET, x - start, x - start));
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {side} {side}\" shape-rendering=\"crispEdges\"><rect width=\"{side}\" height=\"{side}\" fill=\"#fff\"/><path fill=\"#000\" d=\"{path}\"/></svg>"
    )
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

    #[test]
    fn svg_output() {
        let m = encode(&pairing_payload("adbqr-abcdefgh", "0123456789")).unwrap();
        let svg = to_svg(&m);
        let side = m.width + 2 * QUIET;
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains(&format!("viewBox=\"0 0 {side} {side}\"")));
        // The top-left finder pattern starts with a 7-module run at the quiet zone's edge.
        assert!(svg.contains(&format!("M{QUIET} {QUIET}h7v1h-7z")));
    }
}
