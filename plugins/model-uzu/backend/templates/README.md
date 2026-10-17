# Chat templates shipped with the model plugin

`qwen3.8.jinja` renders every prompt (all variants; the Qwen3.5-4B test model too, so tests exercise
what the 27B renders). It is compiled into the plugin binary and rendered with minijinja the way
Hugging Face's `apply_chat_template` renders (trim and lstrip blocks, Python-style `tojson`, Python
string methods; see `src/template.rs`). Mirai's model directories embed the official Qwen3.8 template
in `config.json` (`token_codec_config.prompt_template`); this one is Unsloth's fix of it plus one
change, below. The token sequence the plugin feeds uzu is this template's output tokenized with the
model's own `tokenizer.json`.

Base: the template embedded in `unsloth/Qwen3.8-27B-GGUF@4ca7207` (Unsloth's fix of the official
Qwen3.8 template: developer role, merged leading system messages, tool-call rendering). One change:

- A `system` or `developer` message that appears after the conversation started is rendered as its
  own `<|im_start|>system ... <|im_end|>` turn. The original raises
  `System message must be at the beginning.`, which fails the whole request (llama.cpp issue
  #27107: harnesses insert system notes mid-conversation, and llama.cpp's template probing can hit
  the same assertion).

Every conversation the original accepts renders byte-identically: `src/template.rs`
(`renders_like_the_official_template_the_model_ships`) renders conversations with and without tools,
reasoning and tool calls, thinking on and off, through both this file and the official template
read from the installed model's `config.json`, and compares the text.

Template variables the plugin sets from `Thinking { enabled, effort }` (see `src/template.rs`):
`enable_thinking`, `reasoning_effort` (`low` | `medium` | `high` = `xhigh` | `xhigh`; omitted =
the template's default `xhigh`) and `preserve_thinking = true` (earlier turns keep their reasoning,
so a re-rendered history stays byte-identical to what was generated and the prefix cache holds).
The effort sentence this template writes at the start of the system turn is rendered at the end of
the last message instead (`ChatTemplate::rendering`; the plugin README's "Effort last"), so the
system turn is the same at every effort.
