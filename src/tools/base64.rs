//! Standard padded Base64 for UTF-8 text tools.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

pub(super) fn encode(text: &str) -> String {
    STANDARD.encode(text)
}

pub(super) fn decode(text: &str) -> Result<String, String> {
    // Pasted Base64 often wraps across lines. Ignore only ASCII whitespace;
    // the engine still rejects malformed data and unexpected characters.
    let compact: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let bytes = STANDARD
        .decode(compact)
        .map_err(|error| format!("Invalid Base64: {error}"))?;
    String::from_utf8(bytes).map_err(|_| "Decoded bytes are not valid UTF-8 text".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_unicode_and_accepts_wrapped_base64() {
        let original = "Hello, 🌍\nCafé";
        let encoded = encode(original);
        assert_eq!(decode(&encoded), Ok(original.to_owned()));
        let wrapped = format!("  {}\n{}  ", &encoded[..8], &encoded[8..]);
        assert_eq!(decode(&wrapped), Ok(original.to_owned()));
    }

    #[test]
    fn reports_malformed_and_non_text_data() {
        assert!(decode("???").unwrap_err().starts_with("Invalid Base64:"));
        assert_eq!(
            decode("/w=="),
            Err("Decoded bytes are not valid UTF-8 text".to_owned())
        );
    }
}
