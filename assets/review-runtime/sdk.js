// This script runs inside an opaque-origin chapter frame. It has no native
// token, repository access, decision API, or privileged shell references.
(() => {
  const send=(type,id)=>parent.postMessage({channel:'devcroft-chapter-v1',type,id},'*');
  Object.defineProperty(window,'Devcroft',{value:Object.freeze({
    showEvidence:id=>send('evidence',id),focusClaim:id=>send('claim',id),
    askAbout:id=>send('question',id),proposeFinding:id=>send('finding',id)
  }),writable:false});
  document.addEventListener('click',e=>{const el=e.target.closest('[data-evidence]');if(el)send('evidence',el.dataset.evidence);});
  let reported=false;
  const failed=message=>{if(!reported){reported=true;send('error',String(message).slice(0,600));}};
  window.addEventListener('error',e=>failed(e.message||'Visual script failed.'));
  window.addEventListener('unhandledrejection',()=>failed('The visualization encountered an unhandled error.'));
  document.addEventListener('securitypolicyviolation',()=>failed('The visualization requested an unsupported resource or capability.'));
})();
