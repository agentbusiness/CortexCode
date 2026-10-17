# Web: HTML and CSS (`lang-web`)

Everything CortexCode knows about HTML pages and CSS / SCSS / Less stylesheets, as one `lang@1`
plugin (the `lang` family, see [docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code
graph's facts of pages and stylesheets, vscode-html-language-server and
vscode-css-language-server for diagnostics, definition, references and hover, the project's own
html-validate and stylelint as verifiers, and the web skills. It is a `LanguagePack` served by
`cc-lang-kit`, built like the reference plugin `lang-rust`, with lang-docker's LSP-router approach
for its two servers. (Visual design, component accessibility and checking a rendered UI are
`skills-design`; React / TSX is `lang-typescript`.)

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-web`: category `lang-web`, contract `lang@1`; `cc-lang-web lsp-router` is the language-server router |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |
| `verify/` | gate script, run against the real frame |

## What it serves

Methods are called on the category: `lang-web.describe`, `lang-web.extract`, ...

- **Languages**: `html` (symbol-id prefix `html`; extensions `html`, `htm`), `css` (prefix `css`;
  `css`), `scss` (prefix `scss`; `scss`) and `less` (prefix `less`; `less`).
- **extract** (`src/html.rs`, tree-sitter-html 0.23.2; `src/css.rs`, tree-sitter-css 0.25.0,
  tree-sitter-scss-modern 0.1.0 and tree-sitter-less 1.0.0; all on the kit's tree-sitter 0.25):
  - HTML: elements with an `id` are `variable` symbols (qname `#id`); each `<form>` is a `class`
    symbol (qname `form#id` / `form:name` / `form`, method and action in `http_method` /
    `route_path`, `enctype=` / `novalidate` decorators) with its named `input` / `select` /
    `textarea` / `button` fields as `field` children (also those joined with `form="id"`; type,
    value and validation attributes kept); a form with a local action is a request (method, path,
    field names). Headings are the outline: `module` symbols spanning their sections, nested by
    level. `<style>` blocks are `module` symbols (qname `<style>`) whose CSS is extracted in place
    (rules, custom properties, `var()` reads, `url()` imports, positions the page's); inline
    `<script>` blocks are `module` symbols (qname `<script>`, `type_text` its `type`).
    References: `class` tokens (`Instantiate` `.token`), `id` (`Instantiate` `#id`), id references
    (`for`, `form`, `list`, `headers`, `popovertarget`, `aria-labelledby` / `-describedby` /
    `-controls` / ..., `href="#x"`: `Read` of the id). Imports: `script[src]`, `link[href]`,
    `img[src|srcset]`, `source`, `video`, `audio`, `track`, `iframe`, `embed`, `object[data]`,
    `input[src]`, `a[href]`, `area[href]`, SVG `use` / `image`, when the URL names a file under
    `extract`'s root (module = as written, name = `<tag>.<attribute>`, local = the repository path).
  - CSS / SCSS / Less: rules (one symbol per selector key, see below) with their line ranges;
    `@media` / `@supports` / `@container` / `@layer` / `@scope` / `@starting-style` blocks as
    `module` symbols (qname `@media <prelude>`, everything inside prefixed `<block qname>/`);
    custom properties, Sass `$x` and Less `@x` variables as `const` symbols with their values (the
    declaring selector as decorator); `var(--x)`, `$x`, `@x` uses as `Read` references;
    `@keyframes n` as a `const` symbol `n` with `animation` / `animation-name` uses as `Read`
    references (animation keywords excluded); Sass `@mixin` / `@function` and Less mixins as
    `function` symbols with `@include` / mixin calls as `Call` references; `@extend` and Less
    `:extend()` targets in the rule symbols' `extends`; `@import`, `@use`, `@forward` and `url()`
    as imports when they name a repository file (Sass partial `_x.scss` and `_index.scss` lookup,
    Less `.less` lookup).
  - URLs resolve as a browser and a static server resolve them (`src/paths.rs`): relative to the
    page (its local `<base href>` when it has one) or, for `url()` / `@import`, to the stylesheet;
    root-absolute URLs (`/css/site.css`) against the nearest ancestor directory of the file that
    holds the target (the document root is not known otherwise); a trailing `/` names
    `index.html`; query and fragment dropped. URLs with a scheme, protocol-relative URLs and
    template syntax (`{{ }}`, `{% %}`, `${ }`, `<% %>`) name no file. Nothing is claimed without a
    root.
- **Server**: vscode-langservers-extracted 4.10.0 (`vscode-html-language-server` and
  `vscode-css-language-server`, VS Code's own servers) run with Node.js 18 or newer over stdio.
  The kit drives one server per workspace, so the plugin's own executable runs as
  `cc-lang-web lsp-router` (`src/router.rs`, lang-docker's router without its Compose parts): it
  starts both, answers `initialize` with their merged capabilities, routes `.css` / `.scss` /
  `.less` documents to the CSS server and everything else to the HTML server, keeps request ids,
  progress tokens and server-to-client requests apart, stamps the servers' unversioned
  diagnostics with the version last sent, and answers stylesheet references without the
  declaration from the CSS server's document highlights (its references always include it). The
  HTML server validates embedded `<style>` and `<script>` only (it has no HTML validation of its
  own); the CSS server validates syntax, unknown properties and its lint rules.
- **install**: `npm install --prefix` of vscode-langservers-extracted@4.10.0 into
  `$CORTEX_HOME/toolchains/vscode-langservers-extracted/4.10.0/`. Nothing global is touched.
- **Verifiers** (both `lint`, both the project's own tools, never downloaded: `npx --no --offline`,
  `requires: npx`): `npx --no --offline -- html-validate {files}` in a project with
  `node_modules/.bin/html-validate`; `npx --no --offline -- stylelint "**/*.css"` in a project with
  a stylelint configuration file (`.stylelintrc*`, `stylelint.config.*`).

## Linking HTML to CSS in the code graph

The names are chosen so that a reference's target name is exactly the name of the symbol it means,
whatever file it is in:

| In a page | Reference | Symbol it names | Where |
|---|---|---|---|
| `class="btn primary"` | `Instantiate` `.btn`, `.primary` | rule key symbols `.btn`, `.primary` (kind `class`) | any stylesheet, a `<style>` block of the page |
| `id="main"` | `Instantiate` `#main` | rule key symbol `#main` (kind `class`) | idem |
| `<label for="email">`, `href="#email"`, `aria-describedby="email"` | `Read` `email` | the element symbol `email` (kind `variable`, qname `#email`, no parent) | the same page |
| `var(--brand)` | `Read` `--brand` | custom property `--brand` (kind `const`, parent an at-rule block or none) | any stylesheet or `<style>` |
| `animation: spin 1s` | `Read` `spin` | keyframes `spin` (kind `const`, qname `@keyframes spin`) | idem |

Rules are one symbol per selector key, so `symbol://.btn` finds every rule that names `.btn`: a
rule `.nav a.btn:hover, #main` yields `.nav`, `a`, `.btn` and `#main`, each with the rule's lines,
code and the selector list as signature. Keys of the subject compound (the element the rule
styles) carry no decorator; ancestor and sibling keys (`.nav` above) carry `context`. Keys inside
`:not()`, pseudo-classes and pseudo-elements are not keys; a rule without keys (`:root`, `*`) is
one `type` symbol named by its selector. Nesting (CSS, SCSS, Less) resolves `&`: `&:hover` in
`.card` names `.card`, `&__title` names `.card__title`, a nested rule without `&` has the enclosing
keys as context. Element keys (`button`) are `type` symbols; elements are not referenced by every
tag in the page (that would be a reference per element).

The memory plugin resolves these facts across files: a page's classes and ids link to the rules
of the stylesheets it brings in (`link.href`, `@import`, `@use`, `@forward`, followed through `local`
paths), then project-wide when exactly one file defines the name; `--x`, `$x`, `@x`, keyframes and
mixins resolve along the import chain first; imports link by their `local` path.

## Build and run

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-web     # -> plugins/lang-web/target/lang-web-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/lang-web --install`). The probe extracts a sample
page through the real `extract`, reads every skill and reports whether the servers can run (absent
servers do not fail the probe).

## Verification

`verify/lang_web.mjs` loads the folder in a real frame and checks every method against truth
computed independently: element, form, field, heading and rule lines found in the fixture text;
the page's class tokens against the stylesheet's rule names; imports of files the script wrote
(a missing image is none); an unknown property and an unclosed block written at known lines, and a
CSS error inside the page's `<style>`, before and after a fix; definition and references of a
custom property; hover on a CSS property and an HTML element; skills read back; the
`lang-web.server` event; refusals (component files, a relative root, a missing file). The servers
must be installed; `node verify/lang_web.mjs --install` lets `lang-web.install` add them first.

## Known limits

- Inline `<script>` blocks are noted, not parsed: lang-typescript's extractor is a function of
  another plugin's crate, and depending on it would break this folder's separability (only
  cc-protocol and cc-lang-kit come from outside). A JavaScript extractor in a shared crate would
  let pages carry their inline scripts' facts.
- Vue, Svelte and Astro components are not handled: their templates bind classes and attributes
  through directives and expressions (`:class`, `class:active`, `{...}`), their styles are scoped,
  and their scripts are TypeScript, so a structural reading of them is not sound with these
  grammars.
- `style="..."` attributes and event-handler attributes (`onclick`) are not parsed.
- tree-sitter-css 0.25.0 reports a parse error for named containers (`@container card (...)`);
  tree-sitter-scss-modern does not parse `#{&}` interpolation in selectors; tree-sitter-less does
  not parse `~"..."` escapes. Facts around them are kept; `parse_errors` counts them.
- The stylelint verifier applies when a configuration file exists; the project may still lack
  `node_modules` (then `npx --no` fails instead of downloading). A stylelint configuration only in
  `package.json` is not seen.
