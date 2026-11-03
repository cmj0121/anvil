//! The fatal error every command returns, and its JSON printer.

use serde::Serialize;

use crate::filestore::StoreError;

/// A fatal error: logged once at `Error`, then the process exits with `code`
/// (1 when a target or check failed, 2 for usage or config).
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub message: String,
    /// A [`crate::trust`] refusal, which no caller may fall back from or retry.
    pub refused: bool,
}

impl Failure {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
            refused: false,
        }
    }

    /// A file or folder others can change (exit 2); `message` names the fix.
    pub fn refusal(message: impl Into<String>) -> Self {
        Self {
            refused: true,
            ..Self::usage(message)
        }
    }

    /// The command itself failed (exit 1): a target, a check or a write.
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
            refused: false,
        }
    }

    /// `self` with the exit status of the signal that ended the run, when
    /// `abort` is one's and `self` is a code 1 failure; else `self`.
    pub fn ended_by(self, abort: Option<crate::cancel::AbortReason>) -> Failure {
        match abort.and_then(crate::cancel::AbortReason::exit_code) {
            Some(code) if self.code == 1 => Failure { code, ..self },
            _ => self,
        }
    }
}

// Config, inventory and path problems are exit 2.
impl From<crate::document::DocumentError> for Failure {
    fn from(err: crate::document::DocumentError) -> Self {
        Failure {
            refused: err.is_refused(),
            ..Failure::usage(err.to_string())
        }
    }
}

impl From<crate::paths::PathsError> for Failure {
    fn from(err: crate::paths::PathsError) -> Self {
        Failure::usage(err.to_string())
    }
}

impl From<crate::inventory::UnknownSel> for Failure {
    fn from(err: crate::inventory::UnknownSel) -> Self {
        Failure::usage(err.to_string())
    }
}

// The editor aborting is a failed command (exit 1); anything else is config.
impl From<crate::editor::EditorError> for Failure {
    fn from(err: crate::editor::EditorError) -> Self {
        let code = match err {
            crate::editor::EditorError::Aborted(_) => 1,
            _ => 2,
        };
        Failure {
            code,
            message: err.to_string(),
            refused: false,
        }
    }
}

// A store refusing an operation is a failed command (exit 1).
impl From<crate::secret::SecretError> for Failure {
    fn from(err: crate::secret::SecretError) -> Self {
        if matches!(err, crate::secret::SecretError::Refused { .. }) {
            Failure::refusal(err.to_string())
        } else {
            Failure::failed(err.to_string())
        }
    }
}

impl From<StoreError> for Failure {
    fn from(err: StoreError) -> Self {
        Self {
            code: err.exit_code(),
            message: err.to_string(),
            refused: false,
        }
    }
}

/// Prints `value` as one line of JSON on stdout; an encoding error becomes
/// `on_err("cannot encode JSON: …")`.
pub fn print_json(value: &impl Serialize, on_err: fn(String) -> Failure) -> Result<(), Failure> {
    let json =
        serde_json::to_string(value).map_err(|err| on_err(format!("cannot encode JSON: {err}")))?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ended_by_a_signal_maps_only_a_code_1_failure() {
        use crate::cancel::AbortReason;
        let code = |failure: Failure, abort| failure.ended_by(abort).code;
        assert_eq!(
            code(Failure::failed("x"), Some(AbortReason::Terminated)),
            143
        );
        assert_eq!(code(Failure::failed("x"), Some(AbortReason::HungUp)), 129);
        assert_eq!(code(Failure::failed("x"), Some(AbortReason::Cancelled)), 1);
        assert_eq!(code(Failure::failed("x"), Some(AbortReason::SaveFailed)), 1);
        assert_eq!(code(Failure::failed("x"), None), 1);
        assert_eq!(code(Failure::usage("x"), Some(AbortReason::Terminated)), 2);
        let failure =
            Failure::failed("apply: 1 of 2 targets failed").ended_by(Some(AbortReason::HungUp));
        assert_eq!(failure.message, "apply: 1 of 2 targets failed");
    }

    #[test]
    fn store_failures_preserve_exit_codes() {
        assert_eq!(Failure::from(StoreError::BadPath("../x".into())).code, 2);
        assert_eq!(Failure::from(StoreError::Symlink("x".into())).code, 2);
        assert_eq!(Failure::from(StoreError::NotFound("x".into())).code, 1);
        assert_eq!(Failure::from(StoreError::Exists("x".into())).code, 1);
    }
}
