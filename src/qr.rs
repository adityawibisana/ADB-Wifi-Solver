use qrcode::{Color, EcLevel, QrCode};

/// Quiet zone around the code, in modules (the QR spec asks for 4).
const QUIET: usize = 4;

/// Payload Android's "Pair device with QR code" scanner expects.
pub fn pairing_payload(service_name: &str, password: &str) -> String {
    format!("WIFI:T:ADB;S:{service_name};P:{password};;")
}

/// A QR code rendered for the console, plus its size in character cells.
pub struct Rendered {
    pub text: String,
    pub columns: usize,
}

/// Renders `payload` with half-block characters: one column per module and
/// two modules per text row, which comes out roughly square in a console font.
///
/// The glyphs draw the *light* modules in the foreground color. With `ansi`
/// the colors are forced to white-on-black so the code scans on any terminal
/// theme; without it, the code assumes a light-on-dark terminal, and `invert`
/// flips it for dark-on-light terminals.
pub fn render(payload: &str, ansi: bool, invert: bool) -> Result<Rendered, String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)
        .map_err(|e| format!("could not build QR code: {e}"))?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * QUIET;

    let is_light = |x: usize, y: usize| -> bool {
        let inside = (QUIET..QUIET + width).contains(&x) && (QUIET..QUIET + width).contains(&y);
        let light = !inside || colors[(y - QUIET) * width + (x - QUIET)] == Color::Light;
        light != invert
    };

    let mut text = String::new();
    for y in (0..size).step_by(2) {
        if ansi {
            text.push_str("\x1b[97;40m");
        }
        for x in 0..size {
            let top = is_light(x, y);
            // An odd last row has no partner; treat it as quiet zone.
            let bottom = if y + 1 < size { is_light(x, y + 1) } else { !invert };
            text.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        if ansi {
            text.push_str("\x1b[0m");
        }
        text.push('\n');
    }
    Ok(Rendered { text, columns: size })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_format() {
        assert_eq!(pairing_payload("adbqr-abc", "pw123"), "WIFI:T:ADB;S:adbqr-abc;P:pw123;;");
    }

    #[test]
    fn render_dimensions() {
        let r = render(&pairing_payload("adbqr-abcdefgh", "0123456789"), false, false).unwrap();
        let lines: Vec<&str> = r.text.lines().collect();
        assert_eq!(lines.len(), r.columns.div_ceil(2));
        assert!(lines.iter().all(|l| l.chars().count() == r.columns));
        // The first row is entirely quiet zone, so it is fully light.
        assert!(lines[0].chars().all(|c| c == '█'));
    }
}
