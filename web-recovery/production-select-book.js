selectBook:async o=>{
 const nav=++__molanBookNav; ++__molanFileNav;
 if(l().bookId&&l().bookId!==o){
  await l().saveCurrentFile(); if(nav!==__molanBookNav)return;
  a({openGroup:"",openName:"",fileContent:"",fileWords:0,saveState:"idle",openBaseline:"",auditPromptedFor:""});
 }
 if(nav!==__molanBookNav)return;
 a({bookId:o,sessionId:"",sessions:[],messages:[],tree:[],onboardingActive:!1,leftTab:"sessions"});
 l().loadBookStyle();
 try{
  const[c,d]=await Promise.all([f_(o),Nt(o)]);if(nav!==__molanBookNav||l().bookId!==o)return;
  const m=c[0]?.id??"";a({sessions:c,tree:d,sessionId:m});
  const f=i2(d);if(f)await l().openFile(f.group,f.name);else a({openGroup:"",openName:"",fileContent:"",fileWords:0});
  if(nav!==__molanBookNav||l().bookId!==o)return;
  if(m)await l().selectSession(m);else a({messages:[]});
  if(nav!==__molanBookNav||l().bookId!==o)return;
  a({centerView:"chat"});pl(l);
 }catch(e){if(nav===__molanBookNav&&l().bookId===o)l().showToast("读取作品失败："+(e instanceof Error?e.message:String(e)));throw e;}
},
