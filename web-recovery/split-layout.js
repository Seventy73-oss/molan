// One layout controller, measured from available center width, not outer window width.
var SPLIT_KEY='molan_split_ratio';
var splitBar=null, splitObserver=null, paneObserver=null, observedStage=null;
function splitRatio(){var v=parseFloat(localStorage.getItem(SPLIT_KEY)||'');return Number.isFinite(v)?Math.max(.2,Math.min(.8,v)):.5;}
function applySplit(){
  var stage=document.querySelector('.centerstage');
  if(!stage)return;
  if(observedStage!==stage){
    if(splitObserver)splitObserver.disconnect();
    if(paneObserver)paneObserver.disconnect();
    paneObserver=new MutationObserver(function(records){if(records.some(function(r){return r.target.matches('.editorpane');}))ensureSplitBar();});
    paneObserver.observe(stage,{attributes:true,attributeFilter:['class'],subtree:true});
    observedStage=stage;
    splitObserver=new ResizeObserver(function(){applySplit();});
    splitObserver.observe(stage);
  }
  var pane=stage.querySelector('.editorpane');
  var open=pane&&pane.classList.contains('is-open');
  var desktop=window.innerWidth>850;
  var width=stage.getBoundingClientRect().width;
  var compact=desktop&&width<900;
  stage.classList.toggle('molan-single-pane',!!compact);
  if(splitBar)splitBar.style.display=desktop&&open&&!compact?'':'none';
  if(!pane)return;
  if(!desktop||!open||compact){pane.style.removeProperty('flex');pane.style.removeProperty('width');return;}
  var available=width-9;
  var editorWidth=Math.max(420,Math.min(available-440,available*(1-splitRatio())));
  var px=Math.round(editorWidth)+'px';
  if(pane.style.width!==px){pane.style.flex='0 0 '+px;pane.style.width=px;}
}
function onSplitDrag(e){
  var stage=document.querySelector('.centerstage');
  if(!stage||stage.classList.contains('molan-single-pane'))return;
  e.preventDefault();
  splitBar.classList.add('is-dragging');
  document.body.style.cursor='col-resize';document.body.style.userSelect='none';
  function move(ev){var rect=stage.getBoundingClientRect();var ratio=(ev.clientX-rect.left)/(rect.width-9);localStorage.setItem(SPLIT_KEY,String(Math.max(.2,Math.min(.8,ratio))));applySplit();}
  function up(){document.removeEventListener('mousemove',move,true);document.removeEventListener('mouseup',up,true);document.body.style.cursor='';document.body.style.userSelect='';if(splitBar)splitBar.classList.remove('is-dragging');}
  document.addEventListener('mousemove',move,true);document.addEventListener('mouseup',up,true);
}
function resetSplit(){localStorage.removeItem(SPLIT_KEY);applySplit();}
function ensureSplitBar(){
  var stage=document.querySelector('.centerstage');
  var pane=stage&&stage.querySelector('.editorpane');
  if(!pane||!pane.classList.contains('is-open')||window.innerWidth<=850){if(splitBar&&splitBar.parentNode)splitBar.remove();applySplit();return;}
  if(!splitBar){splitBar=document.createElement('div');splitBar.id='molan-splitbar';splitBar.title='拖动调整对话 / 文档宽度（双击复位）';splitBar.addEventListener('mousedown',onSplitDrag);splitBar.addEventListener('dblclick',resetSplit);}
  if(splitBar.parentNode!==stage||splitBar.nextSibling!==pane)stage.insertBefore(splitBar,pane);
  applySplit();
}
