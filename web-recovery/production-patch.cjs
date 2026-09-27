// Applied after BookSetupCard replacement, before asset hashing. Fail closed on baseline drift.
const fs=require('node:fs'),path=require('node:path');
function patchProduction(source){
 let s=source;
 const once=(old,next)=>{if(s.split(old).length!==2)throw Error('Production anchor mismatch: '+old.slice(0,90));s=s.replace(old,next);};
 const section=(begin,end,next)=>{const a=s.indexOf(begin),b=s.indexOf(end,a+begin.length);if(a<0||b<0||s.indexOf(begin,a+1)>=0)throw Error('Production section mismatch: '+begin);s=s.slice(0,a)+next+s.slice(b);};
 once('function pl(a){','let __molanBookNav=0,__molanFileNav=0,__molanStateQueue=Promise.resolve(),__molanSaveQueue=Promise.resolve(),__molanSetupPending=false;function pl(a){');
 once('l&&kl("last_state",JSON.stringify({bookId:l,view:o,group:c,name:d,session:m}))','l&&(__molanStateQueue=__molanStateQueue.catch(()=>{}).then(()=>kl("last_state",JSON.stringify({bookId:l,view:o,group:c,name:d,session:m}))))');
 section('selectBook:async','selectSession:async',fs.readFileSync(path.join(__dirname,'production-select-book.js'),'utf8').trim());
 once('selectSession:async o=>{const c=', 'selectSession:async o=>{const __book=l().bookId;const c=');
 once('const f=await pn(o,l().bookId);','const f=await pn(o,__book);');
 once('...h.sessionId===o&&h.messagesLoadingSessionId===o?', '...h.bookId===__book&&h.sessionId===o&&h.messagesLoadingSessionId===o?');
 once('a(f=>f.sessionId===o&&f.messagesLoadingSessionId===o?', 'a(f=>f.bookId===__book&&f.sessionId===o&&f.messagesLoadingSessionId===o?');
 section('openFile:async','openFileByName:',
 'openFile:async(o,c)=>{const __nav=++__molanFileNav,d=l().bookId;await l().saveCurrentFile();if(__nav!==__molanFileNav||l().bookId!==d)return;const m=await Ri(d,o,c);if(__nav!==__molanFileNav||l().bookId!==d)return;a({openGroup:o,openName:c,fileContent:m,fileWords:m.replace(/\\s/g,"").length,saveState:"idle",centerView:"editor",mobileReadMode:!0,openBaseline:m,auditPromptedFor:""}),pl(l)},');
 once('a({genreStyles:c,bookStyle:d})','l().bookId===o&&a({genreStyles:c,bookStyle:d})');
 once('saveCurrentFile:async()=>{const{bookId:o,openGroup:c,openName:d,fileContent:m}=l();','saveCurrentFile:async()=>{if(l().saveState!=="dirty"&&l().saveState!=="error")return;const{bookId:o,openGroup:c,openName:d,fileContent:m}=l();');
 once('const f=await rb(o,c,d,m);a(_=>','const f=await rb(o,c,d,m);if(l().bookId!==o||l().openGroup!==c||l().openName!==d)return;a(_=>');
 once('l().showToast(`保存失败：${f instanceof Error?f.message:String(f)}`)}}},createAssetFile:', 'l().showToast(`保存失败：${f instanceof Error?f.message:String(f)}`);throw f}}},createAssetFile:');
 const saveStart=s.indexOf('saveCurrentFile:async()=>'),saveEnd=s.indexOf('createAssetFile:',saveStart);
 if(saveStart<0||saveEnd<0)throw Error('Missing save action');
 const saveAction=s.slice(saveStart+'saveCurrentFile:'.length,saveEnd-1);
 s=s.slice(0,saveStart)+'saveCurrentFile:()=>{const run='+saveAction+';const job=__molanSaveQueue.catch(()=>{}).then(async()=>{await run();while(l().saveState==="dirty")await run()});__molanSaveQueue=job;return job},'+s.slice(saveEnd);
 // A durable receipt is historical proof, not a live tree membership assertion.
 // scan_tree intentionally excludes the review queue; never call a saved draft deleted.
 once('oe=q&&!U&&!de&&!!o.doc&&!ne.some(', 'oe=q&&!(o.savedDocs?.length)&&!U&&!de&&!!o.doc&&!ne.some(');
 once('oe?"已删除":q?"已保存":ee?', 'oe?"已删除":q?(o.savedDocs?.length?"已保存记录":"已保存"):ee?');
 // Save callbacks only update the document/session from which the action started.
 once('const h=await Nt(d);return a({tree:h}),!0','const h=await Nt(d);return l().bookId===d&&a({tree:h}),!0');
 once('const _=await Nt(c);if(a({tree:_}),f){','const _=await Nt(c);if(l().bookId!==c)return!0;if(a({tree:_}),f){');
 once('a(p=>({messages:p.messages.map(_=>_.id===o.id?{..._,result:f}:_)}))','a(p=>({messages:p.messages.map(_=>_.id===o.id?{..._,result:f}:_),messagesBySession:Object.fromEntries(Object.entries(p.messagesBySession).map(([sid,rows])=>[sid,rows.map(_=>_.id===o.id?{..._,result:f}:_)]))}))');
 once('p=JSON.parse(h);a(g=>({messages:g.messages.map(y=>y.id===o.id?{...y,result:p}:y)}));const _=await Nt(c);','p=JSON.parse(h);a(g=>({messages:g.messages.map(y=>y.id===o.id?{...y,result:p}:y),messagesBySession:Object.fromEntries(Object.entries(g.messagesBySession).map(([sid,rows])=>[sid,rows.map(y=>y.id===o.id?{...y,result:p}:y)]))}));const _=await Nt(c);');
 // Session identity is supplied by the caller, not guessed from background IPC traffic.
 once('Sk=(a,l)=>Z?X("rename_session",{sessionId:a,title:l})','Sk=(a,l,o)=>Z?X("rename_session",{sessionId:a,title:l,bookId:o})');
 once('h_=a=>Z?X("delete_session",{sessionId:a})','h_=(a,l)=>Z?X("delete_session",{sessionId:a,bookId:l})');
 once('await Sk(o,d),','await Sk(o,d,l().bookId),');
 once('await h_(o);const m=await f_(c);','await h_(o,c);const m=await f_(c);if(l().bookId!==c)return;');
 // Every generation keeps a local intent value; an inferred skill is not permission to write.
 once('webSearch:y=!1,clearComposer:k,chain:w})=>','webSearch:y=!1,clearComposer:k,chain:w,writeIntent:__intent="explicit-task"})=>');
 once('files:A,webSearch:m,clearComposer:!0})','files:A,webSearch:m,clearComposer:!0,writeIntent:!f&&Yu(p,c).some(v=>xt(v)==="primary"&&["body","outline"].includes(Qu(v)))?"explicit-task":"preview"})');
 const old='skillSelection:q,humanizeOverride:U';
 // Exactly the adapter request, not context metadata.
 once(old,'skillSelection:q,writeIntent:E?"preview":__intent,humanizeOverride:U');
 once('contextFiles:a.contextFiles,skills:a.skills,skillSelection:a.skillSelection??null,humanizeOverride:', 'contextFiles:a.contextFiles,skills:a.skills,skillSelection:a.skillSelection??null,writeIntent:a.writeIntent??"preview",humanizeOverride:');
 const start=s.indexOf('startBookSetup:async'),tail=s.indexOf('const y=o.trim()',start);
 if(start<0||tail<0)throw Error('Missing setup entry');
 s=s.slice(0,start)+'startBookSetup:async(o,c,d,m,f="",h="")=>{const p=c?.trim(),__setup=await X("begin_book_setup",{title:p||"",genre:h||"构思中",pov:"第三人称",form:d||"",platform:m||"",audience:f||""});if(__setup?.ok!==true||!__setup.book?.id||!__setup.session?.id)throw Error("未能建立共创工作区");const _=__setup.book,g=__setup.session;a(D=>({books:[_,...D.books]}));await l().selectBook(_.id);if(l().bookId!==_.id)return;a({sessionId:g.id,onboardingActive:!0,onboardingSkill:"新书共创",skillSel:{},fileSel:{},leftTab:"sessions"});pl(l);'+s.slice(tail);
 const setupStart=s.indexOf('startBookSetup:async'),setupEnd=s.indexOf('coCreateEntry:',setupStart),setupBody=s.indexOf('=>{',setupStart)+3;
 if(setupStart<0||setupEnd<0||s.slice(setupEnd-2,setupEnd)!=='},')throw Error('Setup lock anchor mismatch');
 s=s.slice(0,setupBody)+'if(__molanSetupPending)return;__molanSetupPending=true;try{'+s.slice(setupBody,setupEnd-2)+'}finally{__molanSetupPending=false;}},'+s.slice(setupEnd);
 // The explicit start action always creates a trusted workspace, never adopts an arbitrary empty book.
 section('coCreateEntry:async','bringSettings:', 'coCreateEntry:async()=>{await l().startBookSetup("","")},');
 s+='\nwindow.__molanCurrentBookId=()=>x.getState().bookId;window.__molanCurrentSessionId=()=>x.getState().sessionId;window.__molanRunChat=(o)=>x.getState().runChat(o);window.__molanNextOutline=()=>x.getState().generateNextOutline();window.__molanNextBody=()=>x.getState().runNextChapter();window.__molanRefreshTree=()=>x.getState().refreshTree();\n';
 return s;
}
module.exports={patchProduction};
