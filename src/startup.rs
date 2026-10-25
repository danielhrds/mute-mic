use crate::audio::wide;
use anyhow::{Result, bail};
use std::path::Path;
use windows::{
    Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Registry::*},
    core::{PCWSTR, w},
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("MuteMic");

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

struct Value {
    kind: REG_VALUE_TYPE,
    bytes: Vec<u8>,
}

pub fn command(executable: &Path, directory: &Path) -> Result<String> {
    fn quote(path: &Path) -> Result<String> {
        let path = path.to_string_lossy();
        if path.trim().is_empty() || path.contains(['"', '\r', '\n']) {
            bail!("The startup path is invalid.");
        }
        let trailing = path
            .chars()
            .rev()
            .take_while(|character| *character == '\\')
            .count();
        Ok(format!("\"{}{}\"", path, "\\".repeat(trailing)))
    }
    let result = format!(
        "{} --startup --data-dir {}",
        quote(executable)?,
        quote(directory)?
    );
    if result.encode_utf16().count() > 260 {
        bail!("The startup command is too long. Move MuteMic to a shorter folder path.");
    }
    Ok(result)
}

fn read(key: HKEY) -> Result<Option<Value>> {
    unsafe {
        let mut length = 0;
        let mut kind = REG_VALUE_TYPE::default();
        let status = RegQueryValueExW(
            key,
            VALUE_NAME,
            None,
            Some(&mut kind),
            None,
            Some(&mut length),
        );
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        status.ok()?;
        let mut bytes = vec![0; length as usize];
        RegQueryValueExW(
            key,
            VALUE_NAME,
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut length),
        )
        .ok()?;
        bytes.truncate(length as usize);
        Ok(Some(Value { kind, bytes }))
    }
}

fn write(key: HKEY, value: Option<&Value>) -> Result<()> {
    unsafe {
        if let Some(value) = value {
            RegSetValueExW(key, VALUE_NAME, None, value.kind, Some(&value.bytes)).ok()?;
        } else {
            let status = RegDeleteValueW(key, VALUE_NAME);
            if status != ERROR_FILE_NOT_FOUND {
                status.ok()?;
            }
        }
    }
    Ok(())
}

pub fn enabled(executable: &Path, directory: &Path) -> Result<bool> {
    let Ok(command) = command(executable, directory) else {
        return Ok(false);
    };
    unsafe {
        let mut handle = HKEY::default();
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            KEY_QUERY_VALUE,
            &mut handle,
        );
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        status.ok()?;
        let key = Key(handle);
        let Some(value) = read(key.0)? else {
            return Ok(false);
        };
        if value.kind != REG_SZ || value.bytes.len() % 2 != 0 {
            return Ok(false);
        }
        let characters: Vec<u16> = value
            .bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .take_while(|character| *character != 0)
            .collect();
        Ok(String::from_utf16_lossy(&characters).to_uppercase() == command.to_uppercase())
    }
}

pub fn save(
    executable: &Path,
    directory: &Path,
    enabled: bool,
    save_settings: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let value = if enabled {
        if !executable.is_file() {
            bail!("The executable is unavailable for Windows startup.");
        }
        let command = command(executable, directory)?;
        Some(Value {
            kind: REG_SZ,
            bytes: wide(&command)
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect(),
        })
    } else {
        None
    };
    unsafe {
        let mut handle = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut handle,
            None,
        )
        .ok()?;
        let key = Key(handle);
        let previous = read(key.0)?;
        write(key.0, value.as_ref())?;
        if let Err(error) = save_settings() {
            if let Err(rollback) = write(key.0, previous.as_ref()) {
                return Err(error.context(format!("Startup rollback also failed: {rollback:#}")));
            }
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_command_quotes_spaces_and_trailing_slashes() {
        let text = command(
            Path::new(r"D:\Apps\Mute Mic\MuteMic.exe"),
            Path::new("D:\\Profile Data\\"),
        )
        .unwrap();
        assert_eq!(
            text,
            "\"D:\\Apps\\Mute Mic\\MuteMic.exe\" --startup --data-dir \"D:\\Profile Data\\\\\""
        );
    }
    #[test]
    fn startup_rejects_injected_and_long_commands() {
        assert!(
            command(
                Path::new("D:\\MuteMic.exe\" --bad"),
                Path::new(r"D:\Profile")
            )
            .is_err()
        );
        assert!(
            command(
                Path::new(&format!("D:\\{}\\MuteMic.exe", "a".repeat(230))),
                Path::new(r"D:\Profile")
            )
            .is_err()
        );
    }
}
