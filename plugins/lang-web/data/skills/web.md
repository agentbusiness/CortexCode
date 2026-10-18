HTML and CSS here: get it right, serve and check the page, code that runs, errors and fixes.

## 1. Get it right

1. **Pin down what the page must show and do.** Read the request, then what the project already says about it: the existing pages, stylesheets, scripts, tests and docs.
   - Note each element, text, state and behaviour the request asks for.
   - Keep every id, class and attribute the project or its tests use, exactly: scripts and tests select by them.
   - The request and the project decide, never a file or class name alone.
   - The request and the project disagree: say so and ask, instead of silently picking one.
   - Before coding, note the request's examples: they are the check, their values copied, never computed.
   - Unfamiliar API, format or browser behaviour: look it up (the research plugin, MDN, how others solved it):
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
2. **Write whole blocks, in slices.**
   - Markup showing first. Then each behaviour, checking the page after each.
   - A new page or stylesheet is written whole with `edit` `content`; an existing file is changed with anchored `edits` only, every change to it in one call.
   - Markup: replace the element from opening to closing tag. CSS: the whole rule.
   - Never leave a half tag, an unclosed `<div>` or a lost `}`. One missing `}` breaks every rule after it.
3. **Check it in the browser after every change** (section 2): what is shown, what a click or input changes.
4. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Markup, rules and scripts that work and that the task does not touch stay exactly as they are, unless the user asks to change them. Run the project's tests and validators (section 2); fix the first error first.
5. **Before you finish**, check each requirement again against the served page.

## 2. Run, test, check

Serve over HTTP. `file://` blocks module scripts and `fetch()`, and `/css/a.css` points at the disk root. Start the server in its own `run` with `background: true`:

```bash
python3 -m http.server 8000 --bind 127.0.0.1 --directory site   # the folder with index.html
npm run build                                                   # a build script exists: serve its output (dist/, _site/), not the sources
npm run dev                                                     # or the project's dev server
cx browser network | grep -Ev ' (200|304) '                     # failed assets: 404 is a wrong path
npm test                                                        # the project's tests, if any
npx --no -- html-validate index.html                            # when the project has html-validate
npx --no -- stylelint "css/**/*.css"                            # when it has a stylelint config
```

- `npx canceled due to missing packages`, or no `node_modules`: `npm ci` installs what the project already lists. A new package is a change to the user's project: ask before adding one.
- `read diagnostics://css/site.css` shows CSS syntax errors.
- The HTML server never reports nesting or duplicate ids; html-validate does.
- Stop servers when done: `cx job list`, `cx job kill <id>`.

**Checking your work.** Run only the request's examples against the served page (input and expected values copied from the request, never changed; no case the request does not give). When the project has tests, run them (`npm test`, its end-to-end tests) and add the request's examples there; a project without tests gets them in `tests/`, the request's examples kept there. When the request gives no examples, open every page or control you changed at least once and check what follows from your inputs. Then ask the browser for actual values.
- One line per example, never one assertion that hides the rest.
- A check fails: find out whether the page or the expected value is wrong; when you cannot tell, say so. When a failure shows how the tests use the page (a selector, a text, a value), apply that everywhere it fits.
- Run the tests whole, without `| tail` or `| grep`: the result shows each failure.

```bash
cx browser open http://127.0.0.1:8000/
cx browser console                                                 # must show no errors
cx browser eval 'document.querySelector("h1")?.textContent'        # want: the heading the request names
cx browser eval 'getComputedStyle(document.body).color'            # want: the colour the design uses
cx browser click '#add'
cx browser eval 'document.querySelectorAll(".item").length'        # what the click changed
```

- Fix the first wrong line, reload (`cx browser open` again), re-check.
- After any fix, run every case again, passing ones too.

## 3. Code that runs

Scripts that select elements must run after the elements exist:

```html
<script src="menu.js"></script>          <!-- bad: in <head>, querySelector returns null -->
<script defer src="menu.js"></script>    <!-- good, or type="module" (deferred too) -->
```

Functions in a module are not globals:

```html
<script type="module" src="app.js"></script> <button onclick="save()">   <!-- bad: save is not defined -->
document.querySelector('#save').addEventListener('click', save);          // good, in app.js
```

`querySelector` returns the first match only:

```js
document.querySelector('.item').classList.add('done');                     // bad: only the first item
document.querySelectorAll('.item').forEach((e) => e.classList.add('done')); // good
list.innerHTML += '<li>x</li>';                       // bad: re-creates children, their listeners are lost
list.insertAdjacentHTML('beforeend', '<li>x</li>');   // good; user text: el.textContent = text
```

Specificity decides which rule wins, then order:

```css
#nav a { color: black; }   .active { color: red; }   /* bad: the id wins, .active never shows */
#nav a.active { color: red; }                        /* good: stronger than #nav a */
```

Markup the parser rewrites:

```html
<div class="box"/>                     <!-- bad: an open tag, what follows ends up inside -->
<div class="box"></div>                <!-- good: only void elements self-close (img, input, br) -->
<button>Add row</button>               <!-- bad inside a form: submits and reloads the page -->
<button type="button">Add row</button>
```

- `//` is not a CSS comment; it swallows the next declaration. Use `/* */`.
- `[hidden]` loses to any `display` rule: add `[hidden] { display: none !important; }`.
- `el.value`, `el.checked` set what is shown; `setAttribute('value', v)` only the default.
- `url()` and `@import` resolve against the stylesheet's URL, not the page's.
- `@import` must come before every rule.
- `width: 100%` plus padding overflows: `box-sizing: border-box`.

## 4. Errors

| Error | Fix |
|---|---|
| `Cannot read properties of null (reading 'addEventListener')` | the element does not exist yet (`defer`) or the selector does not match the markup |
| `Uncaught ReferenceError: save is not defined` | handler in a module script: `addEventListener` in the module |
| `Cannot use import statement outside a module` | `<script type="module">` |
| `Failed to resolve module specifier "x"` | browsers need paths: `./x.js`; packages need the project's bundler |
| `Failed to load module script ... MIME type "text/html"`, `Refused to apply style` | the URL is a 404: fix the path |
| `blocked by CORS policy` with origin `null` | the page was opened as `file://`: serve it |
| a declaration has no effect, no error | a stronger or later rule wins, an invalid value, or a missing `;` before it |
| `css-semicolonexpected`, `css-rcurlyexpected`, `css-lcurlyexpected`, `css-colonexpected` | the missing `;`, `}`, `{` or `:` is at or before that position |
| `css-propertyvalueexpected`, `css-identifierexpected`, `css-ruleorselectorexpected`, `css-selectorexpected` | a stray character, a `//` comment, or SCSS syntax in a `.css` file |
| `unknownProperties`, `property-no-unknown` | a typo in the property name |
| `no-invalid-double-slash-comments` | `/* */` |
| `no-invalid-position-at-import-rule` | move `@import` above all rules |
| `declaration-block-no-duplicate-properties` | delete the overridden declaration |
| `no-descending-specificity` | put the stronger selector after the weaker one |
| `element-permitted-content`, `element-permitted-parent`, `close-order` | invalid nesting (`<div>` in `<p>`, text in `<table>`) or a tag closed in the wrong order |
| `void-style`, `no-self-closing`, `void-content` | `<input>` as the config asks; never `<div/>`; no `</input>` |
| `no-dup-id`, `no-missing-references` | ids unique; `for`, `aria-labelledby` and `form` must name an existing id |
| `no-implicit-button-type` | `type="button"` or `type="submit"` |
| `missing-doctype`, `doctype-html` | `<!DOCTYPE html>` as line 1 |
