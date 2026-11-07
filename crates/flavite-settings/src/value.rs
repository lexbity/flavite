//! Setting values and their typed accessors.

use std::collections::BTreeMap;

/// A setting value, capable of representing all setting kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// A boolean value.
    Bool(bool),
    /// An integer value.
    Int(i64),
    /// A string value.
    Str(String),
    /// A list of strings.
    List(Vec<String>),
    /// A map of command-id to chord-string (for shortcut overrides).
    ShortcutMap(BTreeMap<String, String>),
}

impl Default for SettingValue {
    fn default() -> Self {
        Self::Bool(false)
    }
}

impl SettingValue {
    /// Returns `true` if this value is a boolean.
    #[must_use]
    pub fn is_bool(&self) -> bool {
        matches!(self, Self::Bool(_))
    }

    /// Returns `true` if this value is an integer.
    #[must_use]
    pub fn is_int(&self) -> bool {
        matches!(self, Self::Int(_))
    }

    /// Returns `true` if this value is a string.
    #[must_use]
    pub fn is_str(&self) -> bool {
        matches!(self, Self::Str(_))
    }

    /// Returns `true` if this value is a list.
    #[must_use]
    pub fn is_list(&self) -> bool {
        matches!(self, Self::List(_))
    }

    /// Returns `true` if this value is a shortcut map.
    #[must_use]
    pub fn is_shortcut_map(&self) -> bool {
        matches!(self, Self::ShortcutMap(_))
    }

    /// Returns the boolean value, or `None` if this is not a boolean.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the integer value, or `None` if this is not an integer.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the string value, or `None` if this is not a string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(v) => Some(v.as_str()),
            _ => None,
        }
    }

    /// Returns the list value, or `None` if this is not a list.
    #[must_use]
    pub fn as_list(&self) -> Option<&Vec<String>> {
        match self {
            Self::List(v) => Some(v),
            _ => None,
        }
    }

    /// Returns the shortcut map value, or `None` if this is not a shortcut map.
    #[must_use]
    pub fn as_shortcut_map(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            Self::ShortcutMap(v) => Some(v),
            _ => None,
        }
    }

    /// Convert to boolean, panicking if not a boolean (for debug use only).
    #[must_use]
    pub fn to_bool(&self) -> bool {
        self.as_bool().expect("expected bool")
    }

    /// Convert to integer, panicking if not an integer (for debug use only).
    #[must_use]
    pub fn to_int(&self) -> i64 {
        self.as_int().expect("expected int")
    }

    /// Convert to string, panicking if not a string (for debug use only).
    #[must_use]
    pub fn to_str(&self) -> &str {
        self.as_str().expect("expected str")
    }

    /// Convert to list, panicking if not a list (for debug use only).
    #[must_use]
    pub fn to_list(&self) -> &Vec<String> {
        self.as_list().expect("expected list")
    }

    /// Convert to shortcut map, panicking if not a shortcut map (for debug use only).
    #[must_use]
    pub fn to_shortcut_map(&self) -> &BTreeMap<String, String> {
        self.as_shortcut_map().expect("expected shortcut map")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bool_accessors() {
        let value = SettingValue::Bool(true);
        assert!(value.is_bool());
        assert!(!value.is_int());
        assert_eq!(value.as_bool(), Some(true));
        assert_eq!(value.as_int(), None);
        assert_eq!(value.to_bool(), true);
    }

    #[test]
    fn test_int_accessors() {
        let value = SettingValue::Int(42);
        assert!(value.is_int());
        assert!(!value.is_bool());
        assert_eq!(value.as_int(), Some(42));
        assert_eq!(value.to_int(), 42);
    }

    #[test]
    fn test_str_accessors() {
        let value = SettingValue::Str("hello".to_owned());
        assert!(value.is_str());
        assert_eq!(value.as_str(), Some("hello"));
        assert_eq!(value.to_str(), "hello");
    }

    #[test]
    fn test_list_accessors() {
        let value = SettingValue::List(vec!["a".to_owned(), "b".to_owned()]);
        assert!(value.is_list());
        assert_eq!(value.as_list(), Some(&vec!["a".to_owned(), "b".to_owned()]));
    }

    #[test]
    fn test_shortcut_map_accessors() {
        let mut map = BTreeMap::new();
        map.insert("note.save".to_owned(), "Mod+S".to_owned());
        let value = SettingValue::ShortcutMap(map);
        assert!(value.is_shortcut_map());
        assert!(value.as_shortcut_map().is_some());
    }
}
