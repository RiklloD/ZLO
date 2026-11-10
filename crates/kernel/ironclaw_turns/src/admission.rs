use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct TurnAdmissionClass(String);

impl<'de> Deserialize<'de> for TurnAdmissionClass {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl TurnAdmissionClass {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_admission_class(&value)?;
        Ok(Self(value))
    }

    pub fn interactive() -> Self {
        Self("interactive".to_string())
    }

    pub fn mission() -> Self {
        Self("mission".to_string())
    }

    pub fn admin_system() -> Self {
        Self("admin_system".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_admission_class(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("turn admission class must not be empty".to_string());
    }
    if value.len() > 128 {
        return Err("turn admission class must be at most 128 bytes".to_string());
    }
    if value.chars().any(|c| c == '\0' || c.is_control()) {
        return Err("turn admission class must not contain control characters".to_string());
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(
            "turn admission class must contain only lowercase ASCII letters, digits, or _"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_class_validation_and_serde_are_fail_closed() {
        for invalid in ["", "Interactive", "has-dash", "has space", "line\nbreak"] {
            assert!(
                TurnAdmissionClass::new(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
        assert!(TurnAdmissionClass::new("a".repeat(129)).is_err());

        let class = TurnAdmissionClass::new("mission_42").expect("valid class");
        assert_eq!(class.as_str(), "mission_42");
        let encoded = serde_json::to_string(&class).expect("serialize class");
        assert_eq!(
            serde_json::from_str::<TurnAdmissionClass>(&encoded).expect("deserialize class"),
            class
        );
        assert!(serde_json::from_str::<TurnAdmissionClass>("\"INVALID\"").is_err());
    }

    #[test]
    fn builtin_admission_classes_are_stable() {
        assert_eq!(TurnAdmissionClass::interactive().as_str(), "interactive");
        assert_eq!(TurnAdmissionClass::mission().as_str(), "mission");
        assert_eq!(TurnAdmissionClass::admin_system().as_str(), "admin_system");
    }
}
