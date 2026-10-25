use std::{collections::HashSet, sync::OnceLock};

#[derive(Default)]
pub struct KeyTracker {
    pressed: HashSet<isize>,
    waiting: bool,
}

impl KeyTracker {
    pub fn reset(&mut self, waiting: bool) {
        self.pressed.clear();
        self.waiting = waiting;
    }

    pub fn remove(&mut self, device: isize) {
        self.pressed.remove(&device);
    }

    pub fn release_wait(&mut self, all_released: bool) {
        // A reset can start with a held key on an unknown keyboard.
        if all_released && self.pressed.is_empty() {
            self.waiting = false;
        }
    }

    pub fn process(&mut self, device: isize, down: bool) -> bool {
        if self.waiting {
            if down {
                self.pressed.insert(device);
            } else {
                self.pressed.remove(&device);
            }
            return false;
        }
        if down {
            self.pressed.insert(device)
        } else {
            self.pressed.remove(&device);
            false
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct KeyboardPacket {
    pub device: isize,
    pub key: u16,
    pub down: bool,
}

pub fn parse(data: &[u8], pointer_size: usize) -> Option<KeyboardPacket> {
    if !matches!(pointer_size, 4 | 8) {
        return None;
    }
    let header = 8 + pointer_size * 2;
    if data.len() < header + 16 {
        return None;
    }
    let u32_at = |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    let u16_at = |offset| u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
    let size = u32_at(4) as usize;
    if u32_at(0) != 1 || size < header + 16 || size > data.len() {
        return None;
    }
    let key = u16_at(header + 6);
    if key == 0 || key >= 255 || u16_at(header) == 255 {
        return None;
    }
    let device = if pointer_size == 8 {
        i64::from_le_bytes(data[8..16].try_into().unwrap()) as isize
    } else {
        i32::from_le_bytes(data[8..12].try_into().unwrap()) as isize
    };
    Some(KeyboardPacket {
        device,
        key,
        down: u16_at(header + 2) & 1 == 0,
    })
}

pub fn supported_keys() -> &'static [(u16, String)] {
    static KEYS: OnceLock<Vec<(u16, String)>> = OnceLock::new();
    KEYS.get_or_init(build_supported_keys)
}

fn build_supported_keys() -> Vec<(u16, String)> {
    let mut keys: Vec<_> = (0x70..=0x87)
        .map(|key| (key, format!("F{}", key - 0x6f)))
        .collect();
    for (key, label) in [
        (0x13, "Pause"),
        (0x91, "Scroll Lock"),
        (0x2d, "Insert"),
        (0x2e, "Delete"),
        (0x24, "Home"),
        (0x23, "End"),
        (0x21, "Page Up"),
        (0x22, "Page Down"),
        (0x14, "Caps Lock"),
        (0x90, "Num Lock"),
        (0x20, "Space"),
        (9, "Tab"),
        (8, "Backspace"),
        (13, "Enter"),
        (0x26, "Up"),
        (0x28, "Down"),
        (0x25, "Left"),
        (0x27, "Right"),
        (0xbd, "Minus"),
        (0xbb, "Plus"),
        (0xdb, "Open bracket"),
        (0xdd, "Close bracket"),
        (0xdc, "Backslash"),
        (0xba, "Semicolon"),
        (0xde, "Quote"),
        (0xbc, "Comma"),
        (0xbe, "Period"),
        (0xbf, "Slash"),
        (0xc0, "Backtick"),
        (0x6a, "Multiply"),
        (0x6b, "Add"),
        (0x6d, "Subtract"),
        (0x6e, "Decimal"),
        (0x6f, "Divide"),
    ] {
        keys.push((key, label.into()));
    }
    keys.extend((0x60..=0x69).map(|key| (key, format!("NumPad{}", key - 0x60))));
    keys.extend((0x41..=0x5a).map(|key| (key, char::from_u32(key as u32).unwrap().to_string())));
    keys.extend((0x30..=0x39).map(|key| (key, char::from_u32(key as u32).unwrap().to_string())));
    keys
}

pub fn supported(key: u16) -> bool {
    supported_keys().iter().any(|entry| entry.0 == key)
}

pub fn key_label(key: u16) -> &'static str {
    supported_keys()
        .iter()
        .find(|entry| entry.0 == key)
        .map(|entry| entry.1.as_str())
        .unwrap_or("F8")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(pointer: usize, release: bool) -> Vec<u8> {
        let header = 8 + pointer * 2;
        let mut bytes = vec![0; header + 16];
        let length = bytes.len() as u32;
        bytes[..4].copy_from_slice(&1u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes[8..12].copy_from_slice(&1234i32.to_le_bytes());
        bytes[header..header + 2].copy_from_slice(&0x42u16.to_le_bytes());
        bytes[header + 2] = release as u8;
        bytes[header + 6] = 0x77;
        bytes
    }
    #[test]
    fn repeat_requires_release() {
        let mut tracker = KeyTracker::default();
        assert!(tracker.process(1, true));
        for _ in 0..100 {
            assert!(!tracker.process(1, true));
        }
        assert!(!tracker.process(2, false));
        assert!(!tracker.process(1, true));
        assert!(!tracker.process(1, false));
        assert!(tracker.process(1, true));
    }
    #[test]
    fn reset_waits_for_release() {
        let mut tracker = KeyTracker::default();
        tracker.reset(true);
        assert!(!tracker.process(1, true));
        assert!(!tracker.process(1, false));
        tracker.release_wait(true);
        assert!(tracker.process(1, true));
        tracker.remove(2);
        assert!(!tracker.process(1, true));
        tracker.remove(1);
        assert!(tracker.process(1, true));
    }
    #[test]
    fn native_packets_support_both_architectures() {
        for pointer in [4, 8] {
            for release in [false, true] {
                assert_eq!(
                    parse(&packet(pointer, release), pointer),
                    Some(KeyboardPacket {
                        device: 1234,
                        key: 0x77,
                        down: !release
                    })
                );
            }
        }
    }
    #[test]
    fn another_keyboard_cannot_release_the_reset_gate() {
        let mut tracker = KeyTracker::default();
        tracker.reset(true);
        assert!(!tracker.process(1, true));
        assert!(!tracker.process(2, false));
        tracker.release_wait(false);
        assert!(!tracker.process(1, true));
        assert!(!tracker.process(1, false));
        tracker.release_wait(true);
        assert!(tracker.process(1, true));
        assert!(!tracker.process(1, true));
    }
    #[test]
    fn reset_gate_preserves_observed_devices_even_if_async_state_is_released() {
        let mut tracker = KeyTracker::default();
        tracker.reset(true);
        assert!(!tracker.process(1, true));
        assert!(!tracker.process(2, false));
        tracker.release_wait(true);
        assert!(!tracker.process(1, true));
        assert!(!tracker.process(1, false));
        tracker.release_wait(true);
        assert!(tracker.process(1, true));
    }
    #[test]
    fn invalid_packets_are_rejected() {
        let mut bytes = packet(8, false);
        assert!(parse(&[], 8).is_none());
        assert!(parse(&bytes[..39], 8).is_none());
        assert!(parse(&bytes, 16).is_none());
        bytes[4..8].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse(&bytes, 8).is_none());
        bytes = packet(8, false);
        bytes[0] = 0;
        assert!(parse(&bytes, 8).is_none());
        bytes = packet(8, false);
        bytes[30] = 255;
        assert!(parse(&bytes, 8).is_none());
        bytes = packet(8, false);
        bytes[24] = 255;
        assert!(parse(&bytes, 8).is_none());
    }
    #[test]
    fn modifier_chords_are_not_single_keys() {
        assert!(!supported(0x10));
        assert!(!supported(0x11));
        assert!(supported(0x77));
        assert!(supported(0x68));
    }
}
