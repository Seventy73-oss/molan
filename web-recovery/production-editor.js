// Editor adapter aliases come from the pinned EditorPane build.
function Jn(){
 const book=b(s=>s.bookId),name=b(s=>s.openName),group=b(s=>s.openGroup),content=b(s=>s.fileContent),tick=b(s=>s.fileReloadTick);
 const timer=f.useRef(),identity=f.useRef(null);
 const key=JSON.stringify([book,group,name,tick]);
 const save=()=>{const id=identity.current,st=b.getState();if(id&&st.bookId===id.book&&st.openGroup===id.group&&st.openName===id.name)return st.saveCurrentFile().catch(()=>{});};
 const editor=Xe({extensions:[Qe.configure({horizontalRule:!1,codeBlock:!1,code:!1}),Un.configure({html:!1,linkify:!0}),Qn],content:"",immediatelyRender:!1,onUpdate:({editor:e})=>{
  const id=identity.current,st=b.getState();if(!id||st.bookId!==id.book||st.openGroup!==id.group||st.openName!==id.name)return;
  // Update the source document immediately; navigation can now await its pending save.
  st.setFileContent(e.storage.markdown.getMarkdown());clearTimeout(timer.current);timer.current=setTimeout(save,600);
 }});
 f.useEffect(()=>{
  if(!editor||editor.isDestroyed||identity.current?.key===key)return;
  clearTimeout(timer.current);identity.current={book,group,name,key};
  editor.commands.setContent(content||"",!1);editor.view.updateState(Je.create({doc:editor.state.doc,plugins:editor.state.plugins}));
 },[editor,key,content]);
 f.useEffect(()=>()=>{clearTimeout(timer.current);save()},[]);
 return editor;
}
