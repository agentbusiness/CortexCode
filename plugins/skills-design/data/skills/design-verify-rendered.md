Checking a built UI in a browser at phone/desktop widths: overflow, contrast, tap targets, console.

- `read` on a screenshot PNG shows no pixels. Measure the rendered page with `cx browser`.
- Report-only request: run steps 1-5, list findings per viewport.

## 1. Serve, open, read the console

Start the dev server: `run`, `background:true`, `wake.pattern` `localhost:|ready` (`npm run dev`; static files: `python -m http.server 8000`). Then:

```bash
export TAB=$(cx browser open http://localhost:5173/ | awk '{print $2}')   # prints: tab <id> <url> <title>
cx browser console
```

- Fix every console error first. One exception can blank half the page.
- 404s, MIME or module errors: wrong asset paths or server setup.

## 2. Write the audit script once

```bash
cat > /tmp/ui-audit.js <<'EOF'
(()=>{const W=document.documentElement.clientWidth,out=[],seen=new Set(),cs=e=>getComputedStyle(e);
const add=(k,e,m)=>{const id=e.tagName.toLowerCase()+(e.id?'#'+e.id:'')+(e.classList[0]?'.'+e.classList[0]:'');if(!seen.has(k+id)&&out.length<50){seen.add(k+id);out.push(k+' '+id+' '+m)}};
const g=document.createElement('canvas').getContext('2d',{willReadFrequently:true});
const px=c=>{g.clearRect(0,0,1,1);g.fillStyle='#0000';g.fillStyle=c;g.fillRect(0,0,1,1);return g.getImageData(0,0,1,1).data};
const lum=c=>{const[r,G,b]=[...px(c)].slice(0,3).map(v=>(v/=255)<=.04045?v/12.92:((v+.055)/1.055)**2.4);return .2126*r+.7152*G+.0722*b};
const bgOf=e=>{for(;e;e=e.parentElement){const s=cs(e);if(s.backgroundImage!=='none')return null;if(px(s.backgroundColor)[3]>200)return s.backgroundColor}return'white'};
const shown=(s,r)=>r.width>0&&r.height>0&&s.visibility!=='hidden'&&+s.opacity>0;
if(document.documentElement.scrollWidth>W)out.push(`OVERFLOW page ${document.documentElement.scrollWidth}px wide at ${W}px`);
for(const e of document.body.querySelectorAll('*')){const s=cs(e),r=e.getBoundingClientRect(),p=e.parentElement;
 if(r.right>W+1&&p.getBoundingClientRect().right<=W+1&&cs(p).overflowX==='visible')add('OVERFLOW',e,`right edge ${r.right|0}px`);
 if(s.backgroundClip==='text'||s.webkitBackgroundClip==='text')add('SLOP',e,'gradient text');
 if(parseFloat(s.borderLeftWidth)>=3&&parseFloat(s.borderTopWidth)<2&&r.height>30)add('SLOP',e,'side-stripe border-left');
 const t=[...e.childNodes].some(n=>n.nodeType===3&&n.textContent.trim());
 if(t&&+s.opacity===0)add('HIDDEN',e,'text at opacity 0');
 if(!t||!shown(s,r))continue;
 const fs=parseFloat(s.fontSize),bg=bgOf(e);
 if(bg){const a=lum(s.color),b=lum(bg),c=(Math.max(a,b)+.05)/(Math.min(a,b)+.05);if(c<(fs>=24||fs>=18.66&&s.fontWeight>=700?3:4.5))add('CONTRAST',e,`${c.toFixed(2)}:1 ${s.color} on ${bg}`)}
 if(fs<12)add('TINY',e,fs+'px');
 if(/^(P|LI|DD)$/.test(e.tagName)&&r.width/fs/.5>85)add('MEASURE',e,`~${r.width/fs/.5|0} chars/line`)}
for(const e of document.querySelectorAll('a[href],button,input:not([type=hidden]),select,textarea,[role=button]')){const s=cs(e),r=e.getBoundingClientRect();if(!shown(s,r))continue;
 const f=/^(INPUT|SELECT|TEXTAREA)$/.test(e.tagName),min=W<768?44:24;
 if(!f&&!(e.getAttribute('aria-label')||e.getAttribute('aria-labelledby')||e.title||e.innerText.trim()||e.querySelector('img[alt]:not([alt=""])')))add('NAME',e,'no accessible name');
 if(f&&!e.labels?.length&&!e.getAttribute('aria-label')&&!e.getAttribute('aria-labelledby'))add('LABEL',e,'field without label');
 if(f&&W<768&&parseFloat(s.fontSize)<16)add('ZOOM',e,'input font <16px');
 if((r.width<min||r.height<min)&&!/^(checkbox|radio)$/.test(e.type)&&!(e.tagName==='A'&&/^(P|LI|SPAN|TD)$/.test(e.parentElement.tagName)))add('TARGET',e,`${r.width|0}x${r.height|0} <${min}px`)}
document.querySelectorAll('img').forEach(i=>{if(!i.hasAttribute('alt'))add('ALT',i,i.src.slice(-30));if(i.complete&&!i.naturalWidth)add('BROKEN',i,i.src.slice(-30))});
const h=[...document.querySelectorAll('h1,h2,h3,h4,h5,h6')].map(x=>+x.tagName[1]);
if(h.filter(x=>x===1).length!==1)out.push('HEADINGS h1 count '+h.filter(x=>x===1).length);
h.forEach((x,i)=>{if(i&&x>h[i-1]+1)out.push(`HEADINGS h${h[i-1]} then h${x}`)});
out.push(`INFO ${W}x${innerHeight}, ${out.length} findings, loaded fonts: ${[...new Set([...document.fonts].filter(f=>f.status==='loaded').map(f=>f.family))].join(', ')||'none'}`);
return out})()
EOF
```

One line per finding:
- `OVERFLOW`: sideways scroll, with the culprit element. `CONTRAST`: ratio and colors.
- `TINY`: text under 12px. `MEASURE`: line too long. `HIDDEN`: text stuck at opacity 0.
- `SLOP`: gradient text, side stripe.
- `NAME`: control without accessible name. `LABEL`: field without label.
- `ZOOM`: input font under 16px on a phone. `TARGET`: hit area too small.
- `ALT`, `BROKEN`: image. `HEADINGS`: h1 count, skipped levels.
- `INFO`: viewport and loaded web fonts.

## 3. Audit phone and desktop

```bash
for vp in '390,844,true' '1440,900,false'; do IFS=, read w h m <<<"$vp"
  cx call browser.cdp '{"tab_id":"'$TAB'","method":"Emulation.setDeviceMetricsOverride","params":{"width":'$w',"height":'$h',"deviceScaleFactor":2,"mobile":'$m'}}' >/dev/null
  cx call browser.reload '{"tab_id":"'$TAB'"}' >/dev/null; cx browser eval --tab-id $TAB @/tmp/ui-audit.js; done
```

Page with themes or motion: emulate dark mode and reduced motion, run the loop again, confirm the content shows:

```bash
cx call browser.cdp '{"tab_id":"'$TAB'","method":"Emulation.setEmulatedMedia","params":{"features":[{"name":"prefers-color-scheme","value":"dark"},{"name":"prefers-reduced-motion","value":"reduce"}]}}'
```

## 4. Keyboard and structure

`cx browser snapshot` prints the accessibility tree. Defects: a `button` or `link` without a quoted name, a jumping heading order, an open dialog missing from the tree.

Walk the focus with Tab:

```bash
for i in 1 2 3 4 5 6; do cx call browser.press '{"tab_id":"'$TAB'","key":"Tab"}' >/dev/null
  cx browser eval '(e=>[e.tagName,e.innerText?.slice(0,30),getComputedStyle(e).outlineStyle,getComputedStyle(e).boxShadow])(document.activeElement)'; done
```

- Each stop needs a visible ring (`outlineStyle` not `none`, or a ring `boxShadow`), in visual order.
- Exercise interactions: `cx browser click <ref=N|selector>`, `cx browser type <ref=N|selector> <text>`. Then a new snapshot.

## 5. Read the copy

`cx browser eval 'document.body.innerText.slice(0,1500)'`. Fix leftover lorem ipsum, "TODO", duplicated text.

## 6. Fix once, audit once more

1. Fix all findings in one batch with `skill://skills-design/ui-design`, section by finding:
   - `CONTRAST`: Color. `OVERFLOW`, `TARGET`: Layout.
   - `NAME`, `LABEL`, focus: States and accessibility. `SLOP`: Generic look.
2. Run steps 3 and 4 once more.
3. Explain any finding you keep. Then stop polishing.

## 7. Clean up

- Undo the emulation: `browser.cdp` with `Emulation.clearDeviceMetricsOverride`, and `Emulation.setEmulatedMedia` with `"params":{"features":[]}`.
- Stop the dev server: `cx job list`, `cx job kill <id>`.
- The task's checks stay the project's build, typecheck and lint commands.
