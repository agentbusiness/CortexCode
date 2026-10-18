# Chat templates shipped with the model plugin

`qwen3.8.jinja` is the uzu plugin's template, unchanged (see `plugins/model-uzu/backend/templates/README.md`):
the official Qwen3.8 template with Unsloth's fixes, plus a system message after the conversation
started rendered as its own system turn. Bonsai 2 27B's GGUF carries the official Qwen3.8 template
byte for byte (`tokenizer.chat_template`), so every conversation the official template accepts
renders identically here. The token sequence the engine reads is this template's output tokenized
with `tokenizer.json`.

Bonsai's template accepts the efforts `xhigh` (default), `medium` and `low`. Ours maps `high` to
`xhigh`. Its `low` thinks about as long as `xhigh` (PrismML's measurement), so the plugin renders
`low` as `medium` (`runtime::rendered_thinking`); the thinking budget still follows the requested
effort.
