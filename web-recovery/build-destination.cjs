const fs=require('fs'),path=require('path'),crypto=require('crypto');
const root=__dirname,src=path.join(root,'../web-reviewed');
const out=path.resolve(process.argv[2] || path.join(root,'../../destination-review/web'));
if(fs.existsSync(out))throw Error('Output exists: inspect and move it before rebuilding');
const read=p=>fs.readFileSync(p,'utf8');
let bundle=read(path.join(src,'assets/index-molan-6af43e72544b.js'));
const start=bundle.indexOf('function A2({msg:a}){'),end=bundle.indexOf('const O2=',start);
if(start<0||end<0)throw Error('Card anchor mismatch');
bundle=bundle.slice(0,start)+read(path.join(root,'BookSetupCard.js'))+bundle.slice(end);
bundle=require('./production-patch.cjs').patchProduction(bundle);
let ui=read(path.join(src,'molan-ui-patch.js'));
const begin=ui.indexOf("  var SPLIT_KEY = 'molan_split_ratio';"),finish=ui.indexOf('  // ================= 4.',begin);
if(begin<0||finish<0)throw Error('Splitter anchor mismatch');
ui=ui.slice(0,begin)+read(path.join(root,'split-layout.js'))+ui.slice(finish);
const extra=read(path.join(root,'destination-layout.css'));
const editorName='EditorPane-Bu6jXu5a-molan-6af43e72544b.js';
const editor=require('./production-editor-patch.cjs').patchEditor(read(path.join(src,'assets',editorName)));
const tag=crypto.createHash('sha256').update(bundle+ui+extra+editor).digest('hex').slice(0,12);
fs.cpSync(src,out,{recursive:true});
const main='index-molan-6af43e72544b.js', css='index-molan-dab89910060a.css';
fs.writeFileSync(path.join(out,'assets',main),bundle);
fs.writeFileSync(path.join(out,'assets',editorName),editor);
fs.appendFileSync(path.join(out,'assets',css),'\n'+extra);
fs.writeFileSync(path.join(out,'molan-ui-patch.js'),ui);
const mapping={};for(const name of fs.readdirSync(path.join(out,'assets'))){if(/\.(js|css)$/.test(name))mapping[name]=name.replace(/\.(js|css)$/,'-dest-'+tag+'.$1');}
mapping['molan-ui-patch.js']='molan-ui-patch-dest-'+tag+'.js';
function rewrite(dir){for(const entry of fs.readdirSync(dir,{withFileTypes:true})){const p=path.join(dir,entry.name);if(entry.isDirectory())rewrite(p);else if(/\.(js|css|html|json)$/.test(entry.name)){let t=read(p);for(const [old,next] of Object.entries(mapping))t=t.split(old).join(next);fs.writeFileSync(p,t);}}}
rewrite(out);
for(const [old,next] of Object.entries(mapping)){const dir=old==='molan-ui-patch.js'?out:path.join(out,'assets');fs.renameSync(path.join(dir,old),path.join(dir,next));}
console.log(JSON.stringify({out,tag,main:mapping[main],css:mapping[css]},null,2));
