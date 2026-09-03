//! Mapping from GPUI keystrokes to Ghostty terminal keys.

use libghostty_vt::key::Key;

pub(crate) fn map_key(key: &str, key_char: Option<&str>) -> Option<(Key, char)> {
    let normalized = key.to_ascii_lowercase();
    let named = match normalized.as_str() {
        "enter" => Some((Key::Enter, '\0')),
        "tab" => Some((Key::Tab, '\0')),
        "backspace" => Some((Key::Backspace, '\0')),
        "delete" => Some((Key::Delete, '\0')),
        "escape" => Some((Key::Escape, '\0')),
        "up" | "arrowup" => Some((Key::ArrowUp, '\0')),
        "down" | "arrowdown" => Some((Key::ArrowDown, '\0')),
        "left" | "arrowleft" => Some((Key::ArrowLeft, '\0')),
        "right" | "arrowright" => Some((Key::ArrowRight, '\0')),
        "home" => Some((Key::Home, '\0')),
        "end" => Some((Key::End, '\0')),
        "pageup" => Some((Key::PageUp, '\0')),
        "pagedown" => Some((Key::PageDown, '\0')),
        "insert" => Some((Key::Insert, '\0')),
        "space" => Some((Key::Space, ' ')),
        "f1" => Some((Key::F1, '\0')),
        "f2" => Some((Key::F2, '\0')),
        "f3" => Some((Key::F3, '\0')),
        "f4" => Some((Key::F4, '\0')),
        "f5" => Some((Key::F5, '\0')),
        "f6" => Some((Key::F6, '\0')),
        "f7" => Some((Key::F7, '\0')),
        "f8" => Some((Key::F8, '\0')),
        "f9" => Some((Key::F9, '\0')),
        "f10" => Some((Key::F10, '\0')),
        "f11" => Some((Key::F11, '\0')),
        "f12" => Some((Key::F12, '\0')),
        _ => None,
    };
    if named.is_some() {
        return named;
    }

    let character = key_char
        .and_then(|value| value.chars().next())
        .or_else(|| normalized.chars().next())?;
    let logical = match character.to_ascii_lowercase() {
        'a' => Key::A,
        'b' => Key::B,
        'c' => Key::C,
        'd' => Key::D,
        'e' => Key::E,
        'f' => Key::F,
        'g' => Key::G,
        'h' => Key::H,
        'i' => Key::I,
        'j' => Key::J,
        'k' => Key::K,
        'l' => Key::L,
        'm' => Key::M,
        'n' => Key::N,
        'o' => Key::O,
        'p' => Key::P,
        'q' => Key::Q,
        'r' => Key::R,
        's' => Key::S,
        't' => Key::T,
        'u' => Key::U,
        'v' => Key::V,
        'w' => Key::W,
        'x' => Key::X,
        'y' => Key::Y,
        'z' => Key::Z,
        '0' => Key::Digit0,
        '1' => Key::Digit1,
        '2' => Key::Digit2,
        '3' => Key::Digit3,
        '4' => Key::Digit4,
        '5' => Key::Digit5,
        '6' => Key::Digit6,
        '7' => Key::Digit7,
        '8' => Key::Digit8,
        '9' => Key::Digit9,
        '-' | '_' => Key::Minus,
        '=' | '+' => Key::Equal,
        '[' | '{' => Key::BracketLeft,
        ']' | '}' => Key::BracketRight,
        '\\' | '|' => Key::Backslash,
        ';' | ':' => Key::Semicolon,
        '\'' | '"' => Key::Quote,
        ',' | '<' => Key::Comma,
        '.' | '>' => Key::Period,
        '/' | '?' => Key::Slash,
        '`' | '~' => Key::Backquote,
        ' ' => Key::Space,
        _ => Key::Unidentified,
    };
    let unshifted = match logical {
        Key::Minus => '-',
        Key::Equal => '=',
        Key::BracketLeft => '[',
        Key::BracketRight => ']',
        Key::Backslash => '\\',
        Key::Semicolon => ';',
        Key::Quote => '\'',
        Key::Comma => ',',
        Key::Period => '.',
        Key::Slash => '/',
        Key::Backquote => '`',
        _ => character.to_ascii_lowercase(),
    };
    Some((logical, unshifted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_named_terminal_keys() {
        assert_eq!(map_key("enter", None), Some((Key::Enter, '\0')));
        assert_eq!(map_key("arrowup", None), Some((Key::ArrowUp, '\0')));
        assert_eq!(map_key("f12", None), Some((Key::F12, '\0')));
    }

    #[test]
    fn maps_printable_keys_to_their_unshifted_identity() {
        assert_eq!(map_key("A", Some("A")), Some((Key::A, 'a')));
        assert_eq!(map_key("?", Some("?")), Some((Key::Slash, '/')));
        assert_eq!(map_key("_", Some("_")), Some((Key::Minus, '-')));
    }
}
