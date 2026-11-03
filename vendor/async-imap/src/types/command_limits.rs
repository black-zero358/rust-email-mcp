/// Allocation limits retained until this command's matching tagged completion.
/// Default command methods retain their existing response budgets.
#[derive(Clone, Copy, Debug)]
pub struct CommandReadLimits {
    pub(crate) body_section_literal_bytes: usize,
}
impl CommandReadLimits {
    /// Cap each BODY section literal before the parser requests its payload.
    /// The cap applies regardless of returned UID, section or offset.
    pub fn body_section_literal_bytes(max_bytes: usize) -> Self {
        Self {
            body_section_literal_bytes: max_bytes,
        }
    }
}
