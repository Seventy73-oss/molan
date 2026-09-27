const fs=require('fs');
module.exports=function patch(s){
 const old='pn=a=>Z?X("list_messages",{sessionId:a}):Promise.resolve(Mt[a]??[])';
 const replacement='pn=(a,l)=>{if(!Z)return Promise.resolve(Mt[a]??[]);const o=x.getState(),c=l||o.sessions.find(d=>d.id===a)?.bookId||(o.sessionId===a?o.bookId:"");if(!c)return Promise.reject(new Error("无法确认会话所属书籍，请重新选择该书会话"));return X("list_messages",{sessionId:a,bookId:c})}';
 if(s.includes(replacement))return s;
 if(s.split(old).length!==2)throw Error('list_messages wrapper anchor mismatch');
 s=s.replace(old,replacement);
 const call='const f=await pn(o);a(h=>({messagesBySession:';
 if(s.split(call).length!==2)throw Error('selectSession anchor mismatch');
 s=s.replace(call,'const f=await pn(o,l().bookId);a(h=>({messagesBySession:');
 const silent='catch{a(f=>f.sessionId===o&&f.messagesLoadingSessionId===o?{messages:[],messagesLoadingSessionId:""}:{})}},createBook:';
 const visible='catch(e){a(f=>f.sessionId===o&&f.messagesLoadingSessionId===o?{messages:[],messagesLoadingSessionId:""}:{});l().showToast("读取会话失败："+(e instanceof Error?e.message:String(e)))}},createBook:';
 if(s.split(silent).length!==2)throw Error('Session error handling anchor mismatch');
 return s.replace(silent,visible);
};
if(require.main===module){const file=process.argv[2];if(!file)throw Error('Pass bundle path');fs.writeFileSync(file,module.exports(fs.readFileSync(file,'utf8')));console.log('session bookId contract patched');}
