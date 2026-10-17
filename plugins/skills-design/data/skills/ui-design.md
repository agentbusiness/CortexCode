Building or restyling a web UI: direction, color, type, layout, motion, states, a11y, generic look.

The user's explicit style request wins over everything below.

## Procedure

1. **Inherit before inventing.** Find the project's tokens and components:
   ```bash
   find . -path '*/node_modules' -prune -o \( -name 'tailwind.config.*' -o -name 'globals.css' -o -name '*token*' -o -name 'theme.*' -o -name 'DESIGN.md' \) -print | head
   grep -rhoE --include='*.css' --exclude-dir=node_modules --exclude-dir=dist -e '--[a-z0-9-]+:' . | sort | uniq -c | sort -rn | head -30
   ls src/components/ui components/ui 2>/dev/null
   ```
   - A page in an existing product reuses its tokens, type steps, radii and components (`Button`, `Card`, `Dialog`). No new font, color or radius.
   - A redesign may change the look; it keeps content, routes and behavior.
2. **Plan a new page or redesign** at the top of the main stylesheet. Re-read it before later edits:
   ```css
   /* DIRECTION
      scene: <who, where, what light>  -> light | dark
      palette: bg, surface, text, muted, accent (+ success/warning/danger)
      type: display <stack>, body <stack>, ratio, body 16px/1.6
      layout: <one sentence; where the primary action sits>
      signature: <the one memorable move> */
   ```
   - Product UI (dashboard, settings, editor): task speed. One sans family, type ratio 1.125-1.2, neutrals plus one accent, motion at most 250 ms, standard nav and tables.
   - Marketing (landing, pricing): the first viewport says what it is, why it matters, and holds the real action (signup field, search), not a link to it.
   - Reading pages: a 60-75ch column, 16-18px body.
   - Light or dark follows the scene, not the category: on-call engineer at night = dark; accountant in an office = light.
   - Color and type from the subject's own world (its documents, materials, signage), not the category's cliche.
   - Boldness once: one memorable element, everything around it quiet.
   - Real, specific copy and plausible data. Mark invented prices, customers, testimonials or metrics `TODO: replace`, and tell the user.
3. **Tokens.** Write the color, type, space, motion and z-index tokens below. Components use only token names, never hex or `bg-blue-600`.
4. **Build phone first**, then add `@media (min-width: ...)` where the content breaks.
5. **Design every state** of every interactive component and view ("States and accessibility").
6. **Verify rendered**: `skill://skills-design/design-verify-rendered`. Fix all findings in one batch, audit once more, stop.

## Color

```css
:root {
  --hue: 155;
  color-scheme: light dark;
  --bg:        light-dark(oklch(99% .004 var(--hue)), oklch(18% .01 var(--hue)));
  --surface:   light-dark(oklch(97% .006 var(--hue)), oklch(22% .012 var(--hue)));
  --fg:        light-dark(oklch(20% .01 var(--hue)),  oklch(93% .01 var(--hue)));
  --muted-fg:  light-dark(oklch(50% .012 var(--hue)), oklch(70% .012 var(--hue)));
  --border:    light-dark(oklch(90% .008 var(--hue)), oklch(30% .012 var(--hue)));
  --input-border: light-dark(oklch(64% .01 var(--hue)), oklch(55% .01 var(--hue)));
  --primary:    light-dark(oklch(50% .14 var(--hue)), oklch(72% .13 var(--hue)));
  --primary-fg: light-dark(oklch(99% .004 var(--hue)), oklch(18% .01 var(--hue)));
  --ring: var(--primary);
  --danger: light-dark(oklch(50% .19 25), oklch(70% .16 25));
}
[data-theme="light"] { color-scheme: light; }
[data-theme="dark"]  { color-scheme: dark; }
```

- `light-dark()` follows the system setting without JavaScript.
- Manual toggle: set `document.documentElement.dataset.theme`, store it in `localStorage`, apply it from an inline `<head>` script (no flash).
- Tailwind v4: `@theme inline { --color-primary: var(--primary); }`. v3: `colors: { primary: 'var(--primary)' }`. shadcn: edit the `--primary`/`--primary-foreground` pairs in `globals.css`.
- Restrained: tinted neutrals, one accent on at most ~10% of the surface (actions, selection, focus). Never the primary-action color as decoration.

Contrast (WCAG AA) by OKLCH lightness, on a light bg (L >= 97%) or dark bg (L 16-20%):

| Need | Ratio | Light theme | Dark theme |
|---|---|---|---|
| Body and placeholder text | 4.5:1 | L <= 54% | L >= 65% |
| Text >= 24px, or >= 18.7px bold | 3:1 | L <= 62% | L >= 55% |
| Input borders, icons, focus ring | 3:1 | L <= 64% | L >= 55% |
| White text on a filled button | 4.5:1 | fill L <= 52% | dark text on fill L >= 70% |

- Text on a colored surface: same hue, very light or very dark, never neutral gray.
- No alpha on text or borders.
- Never color alone: errors get an icon and text; links in prose are underlined.
- Dark mode is designed, not inverted: elevation = lighter surface; accents lighter, less saturated; no pure black.

## Type

Use the fonts the project ships (`*.woff2`, `@fontsource*`, `next/font`). Network may be off: never add a new font `<link>`. Otherwise a system stack:

| Character | Stack |
|---|---|
| Neutral UI | `system-ui, -apple-system, "Segoe UI", Roboto, sans-serif` |
| Humanist | `Seravek, "Gill Sans Nova", Ubuntu, Calibri, "DejaVu Sans", sans-serif` |
| Reading serif | `Charter, "Bitstream Charter", "Sitka Text", Cambria, serif` |
| Display serif | `Didot, "Bodoni MT", "Noto Serif Display", P052, serif` |
| Code, data | `ui-monospace, "Cascadia Code", "Source Code Pro", Menlo, Consolas, monospace` |

```css
:root {
  --text-sm: .875rem; --text-base: 1rem; --text-lg: 1.25rem; --text-xl: 1.563rem;
  --text-2xl: 1.953rem; --text-3xl: 2.441rem;                    /* ratio 1.25 */
  --text-display: clamp(2.5rem, 1.6rem + 4vw, 5rem);             /* marketing hero only */
}
body { font-size: var(--text-base); line-height: 1.6; }
h1, h2, h3 { line-height: 1.1; letter-spacing: -0.02em; text-wrap: balance; }
p, li { max-width: 68ch; text-wrap: pretty; }
```

- Product UI: fixed `rem` steps, no `clamp()`.
- Body at least 1rem. Nothing functional under .75rem. Inputs at least 16px (iOS zooms smaller ones).
- Neighbouring roles closer than 1.2x read as one role: drop a step, never 15px beside 16px.
- Numbers in tables, prices, timers: `font-variant-numeric: tabular-nums`, right-aligned.

## Layout

```css
:root { --space-1: .25rem; --space-2: .5rem; --space-3: .75rem; --space-4: 1rem; --space-5: 1.5rem;
        --space-6: 2rem; --space-7: 3rem; --space-8: 4rem; --space-9: 6rem;
        --z-dropdown: 100; --z-sticky: 200; --z-overlay: 300; --z-modal: 400; --z-toast: 500; }
.container { width: min(100% - 2rem, 72rem); margin-inline: auto; }
.grid { display: grid; gap: var(--space-5); grid-template-columns: repeat(auto-fit, minmax(min(100%, 18rem), 1fr)); }
.shell { display: grid; grid-template-columns: minmax(0, 1fr); min-height: 100dvh; }
@media (min-width: 48rem) { .shell { grid-template-columns: 16rem minmax(0, 1fr); } }
```

- Space: inside a group 4-12px; between groups 24-32px; between sections 32-48px (product) or 64-128px (marketing).
- Siblings get `gap` on the parent, not margins.
- One filled primary button per view; secondary actions outline or ghost.
- DOM order equals visual order.
- Touch targets 44x44px (`@media (pointer: coarse)`), 24x24px anywhere.
- Hover effects inside `@media (hover: hover)`. Nothing reachable only by hover.
- Phones: nav becomes a drawer or bottom bar, primary action visible. Tables scroll in an `overflow-x: auto` wrapper or become stacked cards. Forms: one column.

| Symptom | Fix |
|---|---|
| Flex child will not truncate, pushes siblings out | `min-width: 0` on that child |
| Grid column overflows | `minmax(0, 1fr)` instead of `1fr` |
| Long URL or word widens the page | `overflow-wrap: anywhere` |
| Dropdown cut off | it sits inside `overflow: hidden`: `popover`, `<dialog>`, a portal or `position: fixed` |
| Full-height section jumps on mobile | `min-height: 100dvh` |
| Sticky header covers jumped-to headings | `scroll-margin-top: <header height>` |

## Motion

```css
:root { --dur-1: 120ms; --dur-2: 200ms; --dur-3: 320ms; --ease-out: cubic-bezier(0.22, 1, 0.36, 1); }
.btn { transition: transform var(--dur-1) var(--ease-out), background-color var(--dur-2) var(--ease-out); }
.btn:active { transform: scale(.97); }
```

- Animate only feedback, change of place, and one entrance on marketing pages.
- Product UI at most 250 ms. No bounce.
- List the properties, never `transition: all`. Animate `transform` and `opacity`, not `width`, `height`, `top`, `margin`.
- Translate, scale, parallax, smooth scrolling only inside `@media (prefers-reduced-motion: no-preference)`.
- Never ship content at `opacity: 0` waiting for a script. Hide only under a class the script sets.

## States and accessibility

| Bad | Good |
|---|---|
| `<div onClick>` | `<button type="button">`; navigation is `<a href>` / `<Link>` |
| `<button><XIcon/></button>` | `<button aria-label="Close dialog"><XIcon aria-hidden="true"/></button>` |
| placeholder as the only label | a visible `<label htmlFor>` |
| `outline: none` | `:focus-visible { outline: 2px solid var(--ring); outline-offset: 2px; }` |
| toggle shown only by color | `aria-pressed` / `aria-expanded` / `aria-current="page"` plus a visible change |

- Every interactive component: default, hover, focus-visible, active, disabled, loading, error.
- A loading button keeps its label, adds a spinner, stays disabled until the request ends.
- Forms: `type`, `autoComplete`, `aria-invalid`, `aria-describedby` pointing at the error text.
- Validate on blur and submit. Focus the first invalid field. Keep what the user typed.
- Overlays: the project's primitives (Radix or shadcn `Dialog`, `Popover`); else `<dialog>` with `showModal()`, focus back to the trigger.
- Every view has:
  - loading: a skeleton shaped like the result;
  - empty: what will appear, plus one action ("No invoices yet. Create your first invoice.");
  - error: what failed, how to fix, Retry;
  - success: past tense of the verb ("Published").
- Buttons say verb + object ("Save changes"), never OK or Submit.
- Test each component with a 60-character name, an empty value and 500 items.

## Generic look: rewrite unless asked for

| Tell | Instead |
|---|---|
| tiny uppercase eyebrow above every heading | delete it |
| three identical icon + title + text cards | one large item and a list, a table, a real screenshot |
| card inside card; border plus shadow everywhere | one container; shadow only on menus and dialogs |
| everything centered | left-aligned text; center only a short hero |
| gradient text, purple gradients, glows, blurred blobs, glass blur | solid color from the palette, a plain surface |
| `border-left: 4px solid accent` stripe | a 1px border with a tinted background, or nothing |
| emoji as icons | the project's SVG icon package |
| "seamless", "empower", "unlock" | what it does: verb + object + number |

```bash
grep -rnoE --include='*.css' --include='*.scss' --include='*.tsx' --include='*.jsx' --include='*.html' --include='*.vue' --include='*.svelte' --exclude-dir=node_modules --exclude-dir=dist 'background-clip: *text|bg-clip-text|border-left: *[2-9]px|border-l-[2-8]\b|(from|via|to)-(purple|violet|indigo)-|transition: *all|transition-all|backdrop-blur' . | head -40
```
