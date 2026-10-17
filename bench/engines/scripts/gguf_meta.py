import sys
from gguf import GGUFReader
r = GGUFReader(sys.argv[1])
for k, f in r.fields.items():
    if k.startswith('tokenizer.ggml.') and k not in ('tokenizer.ggml.model','tokenizer.ggml.pre'): continue
    if k == 'tokenizer.chat_template': print(k, '<len', len(bytes(f.parts[-1])), '>'); continue
    try:
        v = f.contents()
    except Exception:
        v = '?'
    print(k, v if not isinstance(v, list) or len(v) < 20 else f'list[{len(v)}]')
names = [t.name for t in r.tensors]
print('n_tensors', len(names)); print([n for n in names if 'mtp' in n or 'nextn' in n][:10])
print(sorted(set(n.split('.')[2] if n.startswith('blk.') else n for n in names))[:80])
