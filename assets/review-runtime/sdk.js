// This script runs inside an opaque-origin chapter frame. It has no native
// token, repository access, decision API, or privileged shell references.
(() => {
  const send=(type,id)=>parent.postMessage({channel:'devcroft-chapter-v1',type,id},'*');
  Object.defineProperty(window,'Devcroft',{value:Object.freeze({
    showEvidence:id=>send('evidence',id),focusClaim:id=>send('claim',id),
    askAbout:id=>send('question',id),proposeFinding:id=>send('finding',id)
  }),writable:false});
  document.addEventListener('click',e=>{const el=e.target.closest('[data-evidence]');if(el)send('evidence',el.dataset.evidence);});
})();
