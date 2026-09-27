"""Stage versioned web assets; keep dynamic imports on the same main module."""
from pathlib import Path
import hashlib,json,shutil,sys
root=Path(__file__).resolve().parent
src=root/'isolated/web'
out=root/'release-web'
if out.exists(): raise SystemExit('release-web already exists; inspect before replacing')
shutil.copytree(src,out)
patched=(root/'final-bundle.js').read_bytes()
js_old='index-CS9FKb-w.js'; css_old='index-CQ40Vlvh.css'
tag=hashlib.sha256(patched).hexdigest()[:12]
js_new='index-molan-'+tag+'.js'
css_bytes=(out/'assets'/css_old).read_bytes()+b'\n'+(root/'card-layout.css').read_bytes()
(out/'assets'/css_old).write_bytes(css_bytes)
css_new='index-molan-'+hashlib.sha256(css_bytes).hexdigest()[:12]+'.css'
(out/'assets'/js_old).write_bytes(patched)
mapping={f.name:f.stem+'-molan-'+tag+f.suffix for f in (out/'assets').iterdir() if f.suffix in ('.js','.css')}
mapping[js_old]=js_new;mapping[css_old]=css_new
changed=[]
for f in out.rglob('*'):
 if f.is_file() and f.suffix in ('.js','.html','.css','.json'):
  s=f.read_text(encoding='utf8'); n=s
  for old,new in mapping.items(): n=n.replace(old,new)
  if n!=s:f.write_text(n,encoding='utf8');changed.append(str(f.relative_to(out)))
for old,new in mapping.items(): (out/'assets'/old).rename(out/'assets'/new)
print(json.dumps({'js':js_new,'css':css_new,'referencesUpdated':changed},ensure_ascii=False))
