//! Read-only IORegistry access for battery, GPU and disk counters. None of it needs root.

use std::collections::BTreeMap;

pub type Properties = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(i64),
    Text(String),
    Data(Vec<u8>),
    Map(Properties),
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Properties> {
        match self {
            Self::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Data(bytes) => Some(bytes),
            _ => None,
        }
    }
}

#[cfg(target_os = "macos")]
pub use darwin::{entry, services};

#[cfg(not(target_os = "macos"))]
pub fn services(_class: &str) -> Vec<Properties> {
    Vec::new()
}

#[cfg(not(target_os = "macos"))]
pub fn entry(_path: &str) -> Option<Properties> {
    None
}

#[cfg(target_os = "macos")]
mod darwin {
    use std::ffi::{CString, c_void};
    use std::ptr;

    use super::{Properties, Value};

    type CfRef = *const c_void;

    const MAIN_PORT_DEFAULT: u32 = 0;
    const CF_NUMBER_SINT64: isize = 4;
    const CF_STRING_UTF8: u32 = 0x0800_0100;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const libc::c_char) -> CfRef;
        fn IOServiceGetMatchingServices(port: u32, matching: CfRef, iterator: *mut u32) -> i32;
        fn IOIteratorNext(iterator: u32) -> u32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IORegistryEntryFromPath(port: u32, path: *const libc::c_char) -> u32;
        fn IORegistryEntryCreateCFProperties(
            entry: u32,
            properties: *mut CfRef,
            allocator: CfRef,
            options: u32,
        ) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: CfRef);
        fn CFGetTypeID(value: CfRef) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFBooleanGetValue(value: CfRef) -> u8;
        fn CFNumberGetValue(value: CfRef, number_type: isize, out: *mut c_void) -> u8;
        fn CFStringGetLength(value: CfRef) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(
            value: CfRef,
            buffer: *mut libc::c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFDataGetLength(value: CfRef) -> isize;
        fn CFDataGetBytePtr(value: CfRef) -> *const u8;
        fn CFDictionaryGetCount(value: CfRef) -> isize;
        fn CFDictionaryGetKeysAndValues(value: CfRef, keys: *mut CfRef, values: *mut CfRef);
    }

    struct IoObject(u32);

    impl Drop for IoObject {
        fn drop(&mut self) {
            unsafe { IOObjectRelease(self.0) };
        }
    }

    /// The properties of every service of `class`.
    pub fn services(class: &str) -> Vec<Properties> {
        let Ok(class) = CString::new(class) else {
            return Vec::new();
        };
        let mut found = Vec::new();

        unsafe {
            let mut raw_iterator = 0;
            // IOServiceGetMatchingServices releases the matching dictionary.
            let matching = IOServiceMatching(class.as_ptr());
            if IOServiceGetMatchingServices(MAIN_PORT_DEFAULT, matching, &mut raw_iterator) != 0 {
                return found;
            }
            let iterator = IoObject(raw_iterator);

            loop {
                let service = IOIteratorNext(iterator.0);
                if service == 0 {
                    break;
                }
                found.extend(properties(&IoObject(service)));
            }
        }

        found
    }

    /// The properties of the registry entry at `path`, such as `IODeviceTree:/product`.
    pub fn entry(path: &str) -> Option<Properties> {
        let path = CString::new(path).ok()?;
        unsafe {
            match IORegistryEntryFromPath(MAIN_PORT_DEFAULT, path.as_ptr()) {
                0 => None,
                raw_entry => properties(&IoObject(raw_entry)),
            }
        }
    }

    unsafe fn properties(entry: &IoObject) -> Option<Properties> {
        unsafe {
            let mut dictionary = ptr::null();
            if IORegistryEntryCreateCFProperties(entry.0, &mut dictionary, ptr::null(), 0) != 0
                || dictionary.is_null()
            {
                return None;
            }
            let map = convert_dictionary(dictionary);
            CFRelease(dictionary);
            Some(map)
        }
    }

    unsafe fn convert(value: CfRef) -> Option<Value> {
        unsafe {
            let type_id = CFGetTypeID(value);
            if type_id == CFBooleanGetTypeID() {
                Some(Value::Bool(CFBooleanGetValue(value) != 0))
            } else if type_id == CFNumberGetTypeID() {
                let mut number = 0_i64;
                let out = &mut number as *mut i64 as *mut c_void;
                CFNumberGetValue(value, CF_NUMBER_SINT64, out);
                Some(Value::Number(number))
            } else if type_id == CFStringGetTypeID() {
                convert_string(value).map(Value::Text)
            } else if type_id == CFDataGetTypeID() {
                let bytes = CFDataGetBytePtr(value);
                let length = CFDataGetLength(value) as usize;
                if bytes.is_null() {
                    return None;
                }
                Some(Value::Data(
                    std::slice::from_raw_parts(bytes, length).to_vec(),
                ))
            } else if type_id == CFDictionaryGetTypeID() {
                Some(Value::Map(convert_dictionary(value)))
            } else {
                None
            }
        }
    }

    unsafe fn convert_string(value: CfRef) -> Option<String> {
        unsafe {
            let length = CFStringGetLength(value);
            let size = CFStringGetMaximumSizeForEncoding(length, CF_STRING_UTF8) + 1;
            let mut buffer = vec![0_u8; size as usize];
            let out = buffer.as_mut_ptr() as *mut libc::c_char;
            if CFStringGetCString(value, out, size, CF_STRING_UTF8) == 0 {
                return None;
            }
            let end = buffer.iter().position(|&b| b == 0)?;
            String::from_utf8(buffer[..end].to_vec()).ok()
        }
    }

    unsafe fn convert_dictionary(dictionary: CfRef) -> Properties {
        unsafe {
            let count = CFDictionaryGetCount(dictionary) as usize;
            let mut keys = vec![ptr::null(); count];
            let mut values = vec![ptr::null(); count];
            CFDictionaryGetKeysAndValues(dictionary, keys.as_mut_ptr(), values.as_mut_ptr());

            let mut map = Properties::new();
            for (&key, &value) in keys.iter().zip(&values) {
                if CFGetTypeID(key) != CFStringGetTypeID() {
                    continue;
                }
                if let (Some(key), Some(value)) = (convert_string(key), convert(value)) {
                    map.insert(key, value);
                }
            }
            map
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn reads_the_platform_device() {
        let devices = services("IOPlatformExpertDevice");

        assert!(!devices.is_empty());
        assert!(devices[0].contains_key("IOPlatformSerialNumber"));
    }

    #[test]
    fn an_unknown_class_has_no_services() {
        assert!(services("TaertNoSuchClass").is_empty());
    }

    #[test]
    fn an_unknown_path_has_no_entry() {
        assert!(entry("IODeviceTree:/taert-no-such-entry").is_none());
    }
}
