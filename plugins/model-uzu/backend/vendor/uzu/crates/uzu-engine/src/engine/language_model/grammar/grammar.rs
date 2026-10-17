//! CortexCode patch P2: constrained decoding on llguidance.
//!
//! uzu 0.6.7 drives its constraint through xgrammar-rs 0.2.8, whose token masks disagree with its own
//! matcher on byte-level tokens (a mask allowed a token that acceptance then rejected, observed with
//! the Qwen tokenizer on a JSON string rule; see the plugin's `tests/grammar_tokens.rs`), which fails
//! the generation. The same interface is implemented here on llguidance (Lark grammars, which also
//! carry GBNF after the plugin's structural conversion, and JSON Schema), whose masks are computed
//! from the grammar and the token trie the matcher itself consumes.

use std::sync::Arc;

use llguidance::{Matcher, ParserFactory, api::TopLevelGrammar};
use toktrie_hf_tokenizers::ByteTokenizer;

use crate::engine::language_model::grammar::{GrammarConfig, GrammarError, engagement::GrammarEngagementState};

/// Built once per model: the token trie of the vocabulary (the expensive part) and parser settings.
#[derive(Clone)]
pub struct GrammarCompiler {
    factory: Arc<ParserFactory>,
    vocab_size: usize,
    stop_tokens: Vec<u32>,
}

/// CortexCode patch P2: the compiler for a model's `tokenizer.json`, its stop tokens and its
/// (possibly padded) vocabulary size.
pub fn grammar_compiler(
    tokenizer_json: &[u8],
    stop_token_ids: &[u32],
    vocab_size: usize,
) -> Result<GrammarCompiler, GrammarError> {
    let mut tokenizer =
        ByteTokenizer::from_json_bytes(tokenizer_json).map_err(|error| GrammarError::Guidance(error.to_string()))?;
    if !stop_token_ids.is_empty() {
        tokenizer.set_eos_tokens(stop_token_ids);
    }
    let env = tokenizer.into_tok_env(Some(vocab_size)).map_err(|error| GrammarError::Guidance(error.to_string()))?;
    let mut factory = ParserFactory::new_simple(&env).map_err(|error| GrammarError::Guidance(error.to_string()))?;
    factory.quiet();
    Ok(GrammarCompiler {
        factory: Arc::new(factory),
        vocab_size,
        stop_tokens: stop_token_ids.to_vec(),
    })
}

pub struct Grammar {
    words: usize,
    matcher: Matcher,
    engagement_state: GrammarEngagementState,
    stop_tokens: Vec<u32>,
    /// Engaged tokens the matcher did not consume (stop tokens, tokens after it stopped), newest
    /// last, so a rollback removes exactly what the matcher saw.
    consumed: Vec<bool>,
}

impl Grammar {
    pub fn with_compiler(
        config: &GrammarConfig,
        compiler: &GrammarCompiler,
        trigger_token_sequence: Option<Vec<u64>>,
    ) -> Result<Self, GrammarError> {
        let grammar = match config {
            GrammarConfig::JsonSchema {
                schema,
                ..
            } => {
                let value: serde_json::Value =
                    serde_json::from_str(schema).map_err(|error| GrammarError::Guidance(error.to_string()))?;
                TopLevelGrammar::from_json_schema(value)
            },
            GrammarConfig::Regex {
                pattern,
                ..
            } => TopLevelGrammar::from_regex(pattern),
            GrammarConfig::BuiltinJson => TopLevelGrammar::from_json_schema(serde_json::json!({})),
            GrammarConfig::Lark {
                grammar,
            } => TopLevelGrammar::from_lark(grammar.clone()),
        };
        let parser = compiler.factory.create_parser(grammar).map_err(|error| GrammarError::Guidance(error.to_string()))?;
        let matcher = Matcher::new(Ok(parser));
        if let Some(error) = matcher.get_error() {
            return Err(GrammarError::Guidance(error));
        }
        let engagement_state = match trigger_token_sequence.filter(|sequence| !sequence.is_empty()) {
            Some(trigger_sequence) => GrammarEngagementState::Triggered {
                trigger_sequence,
                match_history: Vec::new(),
            },
            None => GrammarEngagementState::Always,
        };
        Ok(Self {
            words: compiler.vocab_size.div_ceil(32),
            matcher,
            engagement_state,
            stop_tokens: compiler.stop_tokens.clone(),
            consumed: Vec::new(),
        })
    }

    /// Fill `bitmask` (one bit per token, set = allowed); false when nothing is constrained.
    pub fn next_bitmask(
        &mut self,
        bitmask: &mut [u32],
    ) -> bool {
        if !self.engagement_state.is_engaged() {
            bitmask.fill(u32::MAX);
            return false;
        }
        let mask = self.matcher.compute_mask_or_eos().map(|mask| mask.as_slice().to_vec());
        bitmask.fill(0);
        match mask {
            Ok(words) => {
                let count = words.len().min(bitmask.len()).min(self.words);
                bitmask[..count].copy_from_slice(&words[..count]);
            },
            // An errored matcher allows nothing but ending the sequence.
            Err(_) => {
                if let Ok(env) = self.matcher.tok_env() {
                    for token in env.tok_trie().eos_token_set().iter() {
                        let (word, bit) = (token as usize / 32, token % 32);
                        if word < bitmask.len() {
                            bitmask[word] |= 1 << bit;
                        }
                    }
                }
            },
        }
        true
    }

    pub fn accept_token(
        &mut self,
        token_id: u64,
    ) -> Result<(), GrammarError> {
        if self.engagement_state.is_engaged() {
            // Stop tokens end the sequence without being part of the grammar (the mask allows
            // them once the grammar can end), and a stopped matcher takes nothing more.
            let consume = !self.matcher.is_stopped() && !self.stop_tokens.contains(&(token_id as u32));
            // A token the grammar rejects must leave the matcher usable: drafted tokens are checked
            // here one by one and most drafts end in a rejected token. `consume_token` puts the
            // matcher into a permanent error state on a rejection (which silently ended every later
            // draft, so speculation never ran under a grammar); `try_consume_tokens` checks first.
            if consume {
                let taken = self.matcher.try_consume_tokens(&[token_id as u32]).map_err(|_| GrammarError::GrammarReject)?;
                if taken == 0 {
                    return Err(GrammarError::GrammarReject);
                }
            }
            self.consumed.push(consume);
        }
        self.engagement_state.accept_token(token_id);
        Ok(())
    }

    pub fn rollback(
        &mut self,
        num_tokens: usize,
    ) {
        let num_grammar_tokens = self.engagement_state.rollback(num_tokens);
        let mut matcher_tokens = 0;
        for _ in 0..num_grammar_tokens {
            if self.consumed.pop() == Some(true) {
                matcher_tokens += 1;
            }
        }
        if matcher_tokens > 0 {
            let _ = self.matcher.rollback(matcher_tokens);
        }
    }

    pub fn is_terminated(&self) -> bool {
        self.matcher.is_stopped()
    }
}
