use thiserror::Error;

#[derive(Debug, Error)]
pub enum GrammarError {
    #[error("Grammar rejected the token")]
    GrammarReject,
    #[error("XGrammar error: {0}")]
    XGrammar(String),
    #[error("Grammar error: {0}")]
    Guidance(String),
}
