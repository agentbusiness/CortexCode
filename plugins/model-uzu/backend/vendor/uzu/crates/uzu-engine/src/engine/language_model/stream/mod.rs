use derive_more::Debug;
use thiserror::Error;

#[cfg(grammar)]
use crate::engine::language_model::grammar::{Grammar, GrammarError};
use crate::{
    backends::common::{Backend, capture_matcher::CaptureError},
    encodable_block::decoder::DecoderError,
    engine::language_model::{LanguageModel, state::LanguageModelState},
    speculators::dflash_tfm::DFlashTreeError,
};
pub use crate::{
    encodable_block::sampling::SamplingMethod, engine::language_model::stream::stream::LanguageModelStream,
};

mod stream;

pub struct LanguageModelStreamOptions {
    pub sampling_method: SamplingMethod,
    #[cfg(grammar)]
    pub grammar: Option<Grammar>,
    /// CortexCode patch P3: draft with the model's speculator when it has one (default true).
    pub speculation: bool,
    /// CortexCode patch P3: seed the penalty window with the state's last tokens (uzu's
    /// behaviour, default true); false penalizes generated tokens only.
    pub penalty_window_includes_state: bool,
    /// CortexCode patch P7: image embeddings placed in this stream's input (prefill only).
    pub images: Option<crate::engine::language_model::images::ImageInput>,
}

#[derive(Debug, Error)]
pub enum LanguageModelStreamError<B: Backend> {
    #[error("Backend error: {0}")]
    Backend(#[source] B::Error),
    #[error("Capture error: {0}")]
    Capture(#[from] CaptureError<B>),
    #[error("Decoder error: {0}")]
    Decoder(#[from] DecoderError<B>),
    #[cfg(grammar)]
    #[error("Grammar error: {0}")]
    Grammar(#[from] GrammarError),
    #[error("Trie accept error: {0}")]
    TrieAccept(#[from] crate::trie::TrieAcceptError),
    #[error("Speculator error: {0}")]
    Speculator(#[from] DFlashTreeError<B>),
    #[error("No seed token (both state and input are empty)")]
    NoSeedToken,
    #[error("Context overflow")]
    ContextOverflow,
    /// CortexCode patch P7.
    #[error("Image input error: {0}")]
    Images(String),
}

impl<B: Backend> LanguageModel<B> {
    pub fn default_stream_options(&self) -> LanguageModelStreamOptions {
        LanguageModelStreamOptions {
            sampling_method: self.default_sampling_method(),
            #[cfg(grammar)]
            grammar: None,
            speculation: true,
            penalty_window_includes_state: true,
            images: None,
        }
    }

    pub fn stream<'a>(
        &'a self,
        input: &[u64],
        state: &'a mut LanguageModelState<B>,
        options: LanguageModelStreamOptions,
    ) -> Result<LanguageModelStream<'a, B>, LanguageModelStreamError<B>> {
        LanguageModelStream::new(self, input, state, options)
    }
}
