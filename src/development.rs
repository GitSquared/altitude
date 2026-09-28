use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevelopmentEnvironment {
    pub name: String,
    pub setup_command: String,
}

impl DevelopmentEnvironment {
    pub fn validate(&self) -> Result<(), DevelopmentEnvironmentError> {
        let mut characters = self.name.chars();
        let valid_first = characters
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        let valid_rest = characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
        if !valid_first || !valid_rest || self.name.len() > 64 {
            return Err(DevelopmentEnvironmentError(format!(
                "environment name {:?} must be 1-64 ASCII letters, numbers, hyphens, or underscores, and start with a letter or number",
                self.name
            )));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct DevelopmentEnvironmentError(String);

impl fmt::Display for DevelopmentEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for DevelopmentEnvironmentError {}
