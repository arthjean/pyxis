use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation};

pub(super) fn nonempty(
    operation: AuxiliaryOperation,
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), AuxiliaryError> {
    if value.trim().is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(AuxiliaryError::invalid(
            operation,
            field,
            format!("expected 1..={max_bytes} printable bytes"),
        ));
    }
    Ok(())
}

pub(super) fn text(
    operation: AuxiliaryOperation,
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), AuxiliaryError> {
    if value.trim().is_empty()
        || value.len() > max_bytes
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(AuxiliaryError::invalid(
            operation,
            field,
            format!("expected 1..={max_bytes} text bytes"),
        ));
    }
    Ok(())
}
