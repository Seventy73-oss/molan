// Compiled-bundle adapter: b=React, s=jsx runtime, x=application store, X=IPC.
// Saving is an explicit action. Never use the currently selected book as an implicit destination.
function A2({msg}) {
  const setup = msg.result.bookSetup;
  const books = x(v => v.books);
  const sourceBookId = b.useRef(x.getState().bookId).current;
  const sourceSessionId = b.useRef(msg.sessionId || x.getState().sessionId).current;
  const [choice, setChoice] = b.useState("");
  const [workspace, setWorkspace] = b.useState(false);
  b.useEffect(()=>{let live=true;X("get_book_setup_context",{sourceBookId,messageId:msg.id}).then(v=>{if(live&&v.eligible){setWorkspace(true);setChoice(old=>old||"source");}}).catch(()=>{});return()=>{live=false;};},[sourceBookId,msg.id]);
  const [title, setTitle] = b.useState(setup.titles?.[0] || "");
  const [busy, setBusy] = b.useState(false);
  const lock = b.useRef(false);
  const [error, setError] = b.useState("");
  const [expanded, setExpanded] = b.useState(new Set());
  const saved = setup.savedFiles || [];
  const bound = setup.destination;
  const groups = ["设定", "细纲", "参考"];
  const labels = {"设定":"设定资料", "细纲":"章节细纲", "参考":"参考拆解"};
  const recommend = file => {
    const name = file.name || "";
    if (/细纲|章纲/.test(name)) return "细纲";
    if (/拆书|拆解|参考/.test(name)) return "参考";
    if (/档案|人物|世界观|伏笔|卷框架|大纲|设定/.test(name)) return "设定";
    return groups.includes(file.group) ? file.group : "";
  };
  const [paths, setPaths] = b.useState(() => setup.files.map((f, index) => {
    const receipt = saved.find(r => r.index === index);
    return {index, group: receipt?.group || recommend(f), name: receipt?.name || f.name};
  }));
  const updatePath = (index, key, value) => setPaths(all => all.map((p,i) => i===index ? {...p,[key]:value} : p));
  const receiptFor = i => saved.find(r => r.index === i);
  const destinationName = bound?.title || (["new","source"].includes(choice) ? title.trim() : books.find(v => v.id === choice)?.title || "");
  const ready = !!bound || !!choice && !!destinationName;
  const updateResult = result => {
    x.setState(st => {
      const map = rows => (rows || []).map(m => m.id === msg.id ? {...m,result} : m);
      const cache = Object.fromEntries(Object.entries(st.messagesBySession).map(([sid,rows]) => [sid,map(rows)]));
      return {messages:map(st.messages),messagesBySession:cache};
    });
  };
  const refreshSource = async () => {
    const rows = await X("list_messages",{bookId:sourceBookId,sessionId:sourceSessionId});
    const current = rows.find(m => m.id===msg.id);
    if(current) updateResult(current.result);
  };
  const save = async indices => {
    if(lock.current || !ready) return;
    lock.current=true;setBusy(true);setError("");
    try {
      const dest = bound ? {mode:"existing",bookId:bound.bookId} : ["new","source"].includes(choice) ? {mode:choice,title:title.trim()} : {mode:"existing",bookId:choice};
      const files = indices.map(index => paths[index]);
      if(files.some(f => !groups.includes(f.group) || !f.name.trim())) throw Error("请确认每项的目录和文件名");
      const result = await X("save_book_setup_selection",{sourceBookId,messageId:msg.id,confirmed:true,destination:dest,files});
      if(result?.ok!==true || !result.result?.bookSetup?.destination?.bookId) throw Error("服务端没有确认保存位置，请刷新后核对");
      updateResult(result.result);
      try {
        const allBooks = await X("list_books",{});
        x.setState({books:allBooks});
        if(result.complete && result.destination.bookId!==sourceBookId && x.getState().bookId===sourceBookId && x.getState().sessionId===sourceSessionId){
          await x.getState().selectBook(result.destination.bookId);
          if(x.getState().bookId===result.destination.bookId && result.destination.sessionId)await x.getState().selectSession(result.destination.sessionId);
        }
        if(x.getState().bookId===result.destination.bookId) {
          const tree=await X("scan_tree",{bookId:result.destination.bookId});
          if(x.getState().bookId===result.destination.bookId)x.setState({tree,treeOpen:true});
        }
      } catch(e) {setError("文件已保存，但目录刷新失败，请重新打开目标作品。");}
    } catch(e) {
      setError(e instanceof Error ? e.message : String(e));
      // A failed multi-file action may already have created a bound destination or saved some files.
      // Reload durable receipts so retry never creates a duplicate work or claims partial completion.
      try {await refreshSource();} catch(ignore) {}
    } finally {lock.current=false;setBusy(false);}
  };
  const openTarget = async () => {
    if(!bound || lock.current)return;
    try {await x.getState().selectBook(bound.bookId);if(bound.sessionId)await x.getState().selectSession(bound.sessionId);}
    catch(e) {setError("打开目标作品失败："+(e instanceof Error?e.message:String(e)));}
  };
  const done = !!bound && setup.saved===true;
  return s.jsxs("section",{className:"booksetup booksetup-v2"+(done?" booksetup--done":""),children:[
    s.jsx("h3",{className:"booksetup-v2__heading",children:done?"资料已保存到指定作品":"先确认目标作品，再保存资料"}),
    s.jsx("p",{className:"booksetup-v2__hint",children:workspace?"确认书名后完成本次新书共创，保留当前会话；不移动或覆盖旧文件。":"只有明确选择目标才保存；不会更名已有作品，不移动或覆盖旧文件。"}),
    bound ? s.jsxs("div",{className:"booksetup-v2__bound",children:[
      s.jsx("strong",{children:"目标作品：《"+bound.title+"》"}),
      s.jsx("span",{children:"本卡已绑定该作品；后续保存保持同一去向。"}),
      s.jsx("button",{className:"btn-ghost",disabled:busy,onClick:openTarget,children:"打开目标作品"})
    ]}) : s.jsxs("div",{className:"booksetup-v2__destination",children:[
      setup.saved && s.jsx("p",{className:"booksetup-v2__hint",children:"这是旧版保存记录，未记录明确的目标作品。重新保存前请核对已有资料；本操作不会清理旧位置。"}),
      s.jsxs("label",{children:["目标作品",s.jsxs("select",{"aria-label":"目标作品",value:choice,disabled:busy,onChange:e=>setChoice(e.target.value),children:[
        s.jsx("option",{value:"",children:"请选择：新建作品或保存到已有作品"}),
        workspace && s.jsx("option",{value:"source",children:"完成当前新书（保留共创会话）"}),
        s.jsx("option",{value:"new",children:"＋ 新建独立作品（不修改当前书）"}),
        ...books.map(book=>s.jsx("option",{value:book.id,children:"已有作品：《"+book.title+"》"},book.id))
      ]})]}),
      ["new","source"].includes(choice) && s.jsxs("label",{children:["新作品书名",s.jsx("input",{"aria-label":"新作品书名",value:title,disabled:busy,onChange:e=>setTitle(e.target.value),placeholder:"输入新作品书名"})]}),
      ready && s.jsx("p",{className:"booksetup-v2__target",children:(choice==="source"?"确认完成本次共创新书：":choice==="new"?"首次点击保存将创建：":"只向以下作品添加资料，不改名：")+"《"+destinationName+"》"})
    ]}),
    error && s.jsx("div",{role:"alert",className:"booksetup-v2__error",children:error}),
    s.jsx("p",{className:"booksetup-v2__hint",children:"目录按文件名建议，请逐项核对。章纲放“章节细纲”；人物、伏笔和卷纲放“设定资料”。"}),
    s.jsx("div",{className:"booksetup__files",children:setup.files.map((file,index)=>{
      const receipt=receiptFor(index), p=receipt || paths[index], isOpen=expanded.has(index);
      return s.jsxs("article",{className:"booksetup__file",children:[
        s.jsxs("div",{className:"booksetup-v2__filetop",children:[
          s.jsx("button",{className:"booksetup__file-toggle",onClick:()=>setExpanded(old=>{const next=new Set(old);next.has(index)?next.delete(index):next.add(index);return next;}),"aria-expanded":isOpen,children:file.label || file.name}),
          receipt ? s.jsx("span",{className:"booksetup__file-done",children:"已保存记录"}) : s.jsx("button",{className:"booksetup__file-save",disabled:busy || !ready,onClick:()=>save([index]),children:busy?"保存中…":"保存此项"})
        ]}),
        s.jsxs("div",{className:"booksetup-v2__path",children:[
          s.jsxs("label",{children:["目录",s.jsxs("select",{"aria-label":"目录："+file.name,value:p.group,disabled:busy || !!receipt,onChange:e=>updatePath(index,"group",e.target.value),children:[s.jsx("option",{value:"",children:"请选择目录"}),...groups.map(g=>s.jsx("option",{value:g,children:labels[g]},g))]})]}),
          s.jsxs("label",{children:["文件名",s.jsx("input",{"aria-label":"文件名："+file.name,value:p.name,disabled:busy || !!receipt,onChange:e=>updatePath(index,"name",e.target.value)})]})
        ]}),
        s.jsx("div",{className:"booksetup-v2__fullpath",children:(destinationName?"《"+destinationName+"》":"目标作品待选")+" / "+(p.group||"目录待选")+" / "+p.name}),
        isOpen && s.jsx("div",{className:"booksetup__full",children:s.jsx(Ii,{remarkPlugins:[Hi],children:file.content})})
      ]},index);
    })}),
    s.jsx("div",{className:"booksetup__foot",children:s.jsx("button",{className:"btn-solid",disabled:busy || !ready || done,onClick:()=>save(setup.files.map((_,i)=>i).filter(i=>!receiptFor(i))),children:busy?"保存中…":done?"全部资料已保存":choice==="new"&&!bound?"确认新建作品并保存全部":"确认保存全部到目标作品"})})
  ]});
}
