const fs=require('fs');
module.exports=s=>{
 const replace=(old,next)=>{if(s.split(old).length!==2)throw Error('Post-review anchor mismatch: '+old.slice(0,80));s=s.replace(old,next)};
 const start=s.indexOf('uN=async a=>'),end=s.indexOf(',z_=a=>{',start);
 if(start<0||end<start)throw Error('uN span missing');
 let un=s.slice(start,end), mock=un.indexOf('for(const l of a.files){const o=St()');
 if(mock>=0)un=un.slice(0,mock)+'throw new Error("保存服务未连接，未写入任何文件")}';
 s=s.slice(0,start)+un+s.slice(end);
 replace('if(!l().messages.some(D=>D.id===o.id))return!0;','if(l().bookId!==m||!l().messages.some(D=>D.id===o.id))return!0;');
 replace('const[D,A]=await Promise.all([Nt(m),ba()]);a({tree:D,books:A,treeOpen:!0})','const[D,A]=await Promise.all([Nt(m),ba()]);if(l().bookId!==m||!l().messages.some(j=>j.id===o.id))return!0;a({tree:D,books:A,treeOpen:!0})');
 replace('const S=h?E.result??{...o.result,bookSetup:{...f,saved:!0}}:o.result;','if(h&&E.result?.bookSetup?.saved!==!0){l().showToast("保存失败：服务端未确认建书状态");return!1}const S=h?E.result:o.result;');
 replace('a(D=>({messages:D.messages.map(A=>A.id===o.id?{...A,result:S}:A)}));','a(D=>({messages:D.messages.map(A=>A.id===o.id?{...A,result:S}:A),messagesBySession:{...D.messagesBySession,[o.sessionId||D.sessionId]:(D.messagesBySession[o.sessionId||D.sessionId]||D.messages).map(A=>A.id===o.id?{...A,result:S}:A)}}));');
 replace('$=async H=>{try{const te=await o(a,void 0,H);te===!0&&S(de=>new Set(de).add(H))}catch{}};','$=async H=>{if(D)return;A(!0);try{const te=await o(a,void 0,H);te===!0&&S(de=>new Set(de).add(H))}catch{}finally{A(!1)}};');
 replace('className:"booksetup__file-save",onClick:()=>$(H.name),children:"保存"','className:"booksetup__file-save",disabled:D,onClick:()=>$(H.name),children:D?"保存中…":"保存"');
 return require('./session-contract-patch.cjs')(s);
};
if(require.main===module){const s=fs.readFileSync(__dirname+'/patched-bundle.js','utf8');fs.writeFileSync(__dirname+'/final-bundle.js',module.exports(s));console.log('Reviewed final bundle generated');}
