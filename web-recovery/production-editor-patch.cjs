const fs=require('node:fs'),path=require('node:path');
function patchEditor(source){const start=source.indexOf('function Jn(){'),end=source.indexOf('const Yn=',start);if(start<0||end<0||source.indexOf('function Jn(){',start+1)>=0)throw Error('Editor hook anchor mismatch');return source.slice(0,start)+fs.readFileSync(path.join(__dirname,'production-editor.js'),'utf8')+source.slice(end)}
module.exports={patchEditor};
