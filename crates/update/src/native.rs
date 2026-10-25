use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An `OsString` serialized losslessly as base64 of the platform's native bytes.
///
/// Unix stores the raw `OsStr` bytes; Windows stores UTF-16LE code units. This
/// lets a job file round-trip arguments that are not valid UTF-8 without
/// panicking or silently replacing characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeString(OsString);

impl NativeString {
    pub fn new(value: impl Into<OsString>) -> Self {
        Self(value.into())
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }

    pub fn contains_nul(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.0.as_bytes().contains(&0)
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            self.0.encode_wide().any(|unit| unit == 0)
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.0.to_string_lossy().contains('\0')
        }
    }
}

impl From<OsString> for NativeString {
    fn from(value: OsString) -> Self {
        Self(value)
    }
}

impl From<&OsStr> for NativeString {
    fn from(value: &OsStr) -> Self {
        Self(value.to_os_string())
    }
}

impl Serialize for NativeString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = base64::engine::general_purpose::STANDARD.encode(encode(self.as_os_str()));
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for NativeString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)?;
        decode(bytes).map(Self).map_err(serde::de::Error::custom)
    }
}

#[cfg(unix)]
fn encode(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(unix)]
fn decode(bytes: Vec<u8>) -> Result<OsString, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes))
}

#[cfg(windows)]
fn encode(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value
        .encode_wide()
        .flat_map(|unit| unit.to_le_bytes())
        .collect()
}

#[cfg(windows)]
fn decode(bytes: Vec<u8>) -> Result<OsString, String> {
    use std::os::windows::ffi::OsStringExt;

    if !bytes.len().is_multiple_of(2) {
        return Err("native string has an odd number of UTF-16 bytes".to_string());
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(OsString::from_wide(&units))
}

#[cfg(not(any(unix, windows)))]
fn encode(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(not(any(unix, windows)))]
fn decode(bytes: Vec<u8>) -> Result<OsString, String> {
    String::from_utf8(bytes)
        .map(OsString::from)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_plain_and_empty_strings() {
        for value in ["", "--demo", "写真"] {
            let native = NativeString::new(value);
            let json = serde_json::to_string(&native).unwrap();
            let decoded: NativeString = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, native);
        }
    }

    #[cfg(unix)]
    #[test]
    fn round_trips_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let native = NativeString::from(OsString::from_vec(vec![b'a', 0x80, b'b']));
        let json = serde_json::to_string(&native).unwrap();
        let decoded: NativeString = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, native);
    }

    #[test]
    fn rejects_invalid_base64() {
        assert!(serde_json::from_str::<NativeString>("\"not base64!\"").is_err());
    }
}
