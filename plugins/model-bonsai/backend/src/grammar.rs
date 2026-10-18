//! Constrained decoding on llguidance: a grammar's token mask applied to the logits before each
//! sample. The same matcher the uzu plugin runs inside uzu (its vendor patch P2), built from the
//! model's `tokenizer.json` (whose ids the load checked against the GGUF vocab), so the mask comes
//! from the same token trie the matcher consumes.
//!
//! With thinking on, a grammar engages after the trigger (`</think>`): reasoning is free.

use llguidance::api::TopLevelGrammar;
use llguidance::{Matcher, ParserFactory};
use serde_json::Value;
use std::sync::Arc;
use toktrie_hf_tokenizers::ByteTokenizer;

/// What a request's output must follow.
#[derive(Debug, Clone, PartialEq)]
pub enum GrammarSpec {
    JsonSchema(Value),
    /// A grammar in llguidance's Lark dialect (GBNF is converted by `constraint.rs`).
    Lark(String),
}

/// Built once per model: the token trie of the vocabulary (the expensive part) and parser settings.
#[derive(Clone)]
pub struct GrammarCompiler {
    factory: Arc<ParserFactory>,
    vocab_size: usize,
    stop_tokens: Vec<u32>,
}

impl GrammarCompiler {
    /// The compiler for a model's `tokenizer.json`, its end-of-generation tokens and its (possibly
    /// padded) vocabulary size.
    pub fn new(tokenizer_json: &[u8], stop_tokens: &[u32], vocab_size: usize) -> Result<Self, String> {
        let mut tokenizer = ByteTokenizer::from_json_bytes(tokenizer_json).map_err(|error| error.to_string())?;
        if !stop_tokens.is_empty() {
            tokenizer.set_eos_tokens(stop_tokens);
        }
        let env = tokenizer.into_tok_env(Some(vocab_size)).map_err(|error| error.to_string())?;
        let mut factory = ParserFactory::new_simple(&env).map_err(|error| error.to_string())?;
        factory.quiet();
        Ok(Self { factory: Arc::new(factory), vocab_size, stop_tokens: stop_tokens.to_vec() })
    }

    pub fn compile(&self, spec: &GrammarSpec, trigger: Option<Vec<u32>>) -> Result<Grammar, String> {
        let grammar = match spec {
            GrammarSpec::JsonSchema(schema) => TopLevelGrammar::from_json_schema(schema.clone()),
            GrammarSpec::Lark(text) => TopLevelGrammar::from_lark(text.clone()),
        };
        let parser = self.factory.create_parser(grammar).map_err(|error| error.to_string())?;
        let matcher = Matcher::new(Ok(parser));
        if let Some(error) = matcher.get_error() {
            return Err(error);
        }
        let engagement = match trigger.filter(|sequence| !sequence.is_empty()) {
            Some(sequence) => Engagement::Triggered { sequence, matched: 0 },
            None => Engagement::Always,
        };
        Ok(Grammar { matcher, engagement, stop_tokens: self.stop_tokens.clone(), vocab_size: self.vocab_size })
    }
}

/// When the grammar starts constraining.
enum Engagement {
    Always,
    /// Dormant until the generated stream contains `sequence`; `matched` is the length of the
    /// longest prefix of it the stream ends with.
    Triggered { sequence: Vec<u32>, matched: usize },
}

impl Engagement {
    fn engaged(&self) -> bool {
        match self {
            Self::Always => true,
            Self::Triggered { sequence, matched } => *matched == sequence.len(),
        }
    }

    fn accept(&mut self, token: u32) {
        if let Self::Triggered { sequence, matched } = self
            && *matched < sequence.len()
        {
            // The longest prefix of `sequence` that is a suffix of `sequence[..matched]` + token.
            let mut length = *matched + 1;
            while length > 0 {
                if sequence[length - 1] == token && sequence[*matched + 1 - length..*matched] == sequence[..length - 1] {
                    break;
                }
                length -= 1;
            }
            *matched = length;
        }
    }
}

pub struct Grammar {
    matcher: Matcher,
    engagement: Engagement,
    stop_tokens: Vec<u32>,
    vocab_size: usize,
}

impl Grammar {
    /// Constrain from the next token on (a forced end of reasoning: the answer starts now).
    pub fn engage_now(&mut self) {
        self.engagement = Engagement::Always;
    }

    /// Mask `logits` to the tokens the grammar allows (nothing changes while it is not engaged). An
    /// errored matcher allows only ending the sequence.
    pub fn apply(&mut self, logits: &mut [f32]) {
        if !self.engagement.engaged() {
            return;
        }
        match self.matcher.compute_mask_or_eos() {
            Ok(mask) => {
                let words = mask.as_slice();
                for (token, logit) in logits.iter_mut().enumerate().take(self.vocab_size) {
                    let word = words.get(token / 32).copied().unwrap_or(0);
                    if word & (1 << (token % 32)) == 0 {
                        *logit = f32::NEG_INFINITY;
                    }
                }
                for logit in logits.iter_mut().skip(self.vocab_size) {
                    *logit = f32::NEG_INFINITY;
                }
            }
            Err(_) => {
                for (token, logit) in logits.iter_mut().enumerate() {
                    if !self.stop_tokens.contains(&(token as u32)) {
                        *logit = f32::NEG_INFINITY;
                    }
                }
            }
        }
    }

    /// Record a sampled (or fed) token. Stop tokens end the sequence without being part of the
    /// grammar, and a matcher that finished takes nothing more. Err: the grammar rejects it.
    pub fn accept(&mut self, token: u32) -> Result<(), String> {
        if self.engagement.engaged() && !self.matcher.is_stopped() && !self.stop_tokens.contains(&token) {
            // `try_consume_tokens` leaves the matcher usable when it rejects (consume_token would
            // put it into a permanent error state).
            let taken = self.matcher.try_consume_tokens(&[token]).map_err(|error| error.to_string())?;
            if taken == 0 {
                return Err(format!("the grammar rejects token {token}"));
            }
        }
        self.engagement.accept(token);
        Ok(())
    }

    /// The grammar is complete: only ending the sequence is allowed.
    pub fn finished(&self) -> bool {
        self.engagement.engaged() && self.matcher.is_stopped()
    }
}
