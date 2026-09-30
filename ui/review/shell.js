(() => {
  'use strict';
  // The capability is delivered only in this trusted top document, never
  // through Wry initialization scripts or chapter messages.
  const token=__CAPABILITY__;
  const $=id=>document.getElementById(id);
  const pending=new Map();let sequence=0,queue=Promise.resolve(),state=null;
  let chapter=null,evidence=null,file=null,pinned=false,whole=false,textMode=false,frame=null;
  let findingAnchor=null,questionAnchor=null,draftTimer=null,sourceRequest=0,localPage=null;
  const stamps=new Map();
  function changed(key,value){const next=JSON.stringify(value);if(stamps.get(key)===next)return false;stamps.set(key,next);return true;}
  const make=(tag,text,cls)=>{const e=document.createElement(tag);if(text!==undefined)e.textContent=text;if(cls)e.className=cls;return e;};
  const button=(text,action,cls)=>{const b=make('button',text,cls);b.type='button';b.addEventListener('click',action);return b;};
  function call(op,data={}) {
    const run=()=>new Promise((resolve,reject)=>{
      const seq=++sequence;pending.set(seq,{resolve,reject});
      window.ipc.postMessage(JSON.stringify({token,seq,op,capture:state?.capture.id,version:state?.version,data}));
    });
    const result=queue.then(run);queue=result.catch(()=>{});return result;
  }
  function error(e){$('status').textContent=e.message||String(e);}
  window.__dcReceive=message=>{
    if(message.state){
      if(state&&state.capture.id!==message.state.capture.id){chapter=null;evidence=null;file=null;pinned=false;whole=false;localPage=null;frame=null;stamps.clear();}
      state=message.state;render();
    }
    if(message.status!==undefined)$('status').textContent=message.status;
    if(message.log!==undefined)$('log').textContent=message.log;
    const request=pending.get(message.seq);
    if(request){pending.delete(message.seq);message.error?request.reject(new Error(message.error)):request.resolve(message.result);}
    if(message.error)error(message.error);
  };
  const active=()=>state?.bundle?.manifest.chapters.find(c=>c.id===chapter);
  const anchor=()=>({chapter,evidence});
  function page(name){
    if(name==='walkthrough'&&!active())name='changes';
    for(const p of ['overview','walkthrough','findings','changes'])$(p).hidden=p!==name;
    document.querySelectorAll('.tabs [data-page]').forEach(b=>b.setAttribute('aria-pressed',String(b.dataset.page===name)));
    localPage=name;
    call('location',{page:name,chapter,evidence}).then(()=>{if(localPage===name)localPage=null;}).catch(error);
  }
  function selectChapter(id){
    if(!state.bundle?.manifest.chapters.some(c=>c.id===id))return;
    chapter=id;textMode=false;pinned=false;whole=false;evidence=active().evidence_ids[0]||null;
    renderChapter();renderOutline();page('walkthrough');
  }
  function renderOutline(){
    $('outline').replaceChildren();$('overview-chapters').replaceChildren();
    const chapters=state.bundle?.manifest.chapters||[];
    chapters.forEach((c,i)=>{
      const marked=state.examined.includes(c.id);
      const b=button(`${marked?'✓':String(i+1).padStart(2,'0')}  ${c.title}`,()=>selectChapter(c.id),'chapter-button');
      b.setAttribute('aria-pressed',String(chapter===c.id));$('outline').append(b);
      const overview=button(c.title,()=>{selectChapter(c.id);$('outline').querySelectorAll('button')[i]?.focus();},'chapter-button');overview.append(make('span',c.summary));$('overview-chapters').append(overview);
    });
    if(!chapters.length)$('outline').append(make('p','Source review is ready. Generate a guide when you want a visual explanation.','muted'));
    $('progress').textContent=`${state.examined.length} of ${chapters.length} behaviors examined`;
    const covered=new Set(chapters.flatMap(c=>c.evidence_ids).map(id=>state.capture.evidence.find(e=>e.id===id)?.path));
    $('unassigned').textContent=`${state.capture.files.filter(f=>!covered.has(f.path)).length} files outside the guide`;
  }
  function render(){
    if(!state)return;
    $('scope').textContent=state.capture.label;
    $('title').textContent=state.bundle?.manifest.title||'Review your changes';
    $('revision').textContent=`${state.capture.branch||'Detached revision'} · base ${state.capture.base.slice(0,12)} · HEAD ${state.capture.head.slice(0,12)} · snapshot ${state.capture.id.slice(0,10)}`;
    $('overview-title').textContent=state.bundle?.manifest.title||'Source review is ready';
    $('overview-summary').textContent=state.bundle?.manifest.summary||'Inspect all captured changes now. Guide generation is optional and never blocks source review.';
    $('summary-button').textContent=`Review findings${state.findings.length?' ('+state.findings.length+')':''}`;
    const selected=$('provider').value||state.provider;
    if(changed('providers',state.providers)){$('provider').replaceChildren();state.providers.forEach(p=>{const option=make('option',p.label);option.value=p.id;option.disabled=!p.eligible;$('provider').append(option);});$('provider').value=selected;}
    $('sharing').textContent=state.sharing;
    $('question-sharing').textContent=state.sharing;
    providerState();$('stop').hidden=!state.busy;
    if(!chapter||!active())chapter=state.chapter||state.bundle?.manifest.chapters[0]?.id||null;
    if(!evidence)evidence=state.evidence||active()?.evidence_ids[0]||null;
    if(changed('outline',[state.bundleHash,state.examined,chapter]))renderOutline();
    if(changed('findings',[state.findings,state.investigations,state.history,state.examined]))renderFindings();
    if(changed('files',state.capture.id))renderFiles();
    // Keep the mounted canvas and its presentation state during saves.
    if($('chapter-canvas').dataset.chapter!==chapter||$('chapter-canvas').dataset.bundle!==state.bundleHash)renderChapter();
    const c=active();$('examined').textContent=state.examined.includes(chapter)?'Reopen behavior':'Mark examined';
    $('decision').textContent=state.examined.includes(chapter)?'Examined by you':'Waiting for your judgment';
    let current=localPage||state.page||'changes';if(current==='walkthrough'&&!c)current='changes';
    for(const p of ['overview','walkthrough','findings','changes'])$(p).hidden=p!==current;
    document.querySelectorAll('.tabs [data-page]').forEach(b=>b.setAttribute('aria-pressed',String(b.dataset.page===current)));
    if($('question-dialog').open)renderThread();
  }
  function renderChapter(){
    const c=active();if(!c)return;
    $('chapter-label').textContent='Behavior · captured interpretation';$('chapter-title').textContent=c.title;$('chapter-summary').textContent=c.summary;
    const canvas=$('chapter-canvas');canvas.replaceChildren();frame=null;canvas.dataset.chapter=c.id;canvas.dataset.bundle=state.bundleHash;
    if(!textMode){
      frame=document.createElement('iframe');frame.title=c.title+' visual explanation';frame.setAttribute('sandbox','allow-scripts');frame.referrerPolicy='no-referrer';
      const theme=document.documentElement.style.colorScheme||'light dark';
      const policy="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";
      frame.srcdoc='<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="'+policy+'"><style>:root{color-scheme:'+theme+';--review-bg:light-dark(#fff,#202323);--review-ink:light-dark(#202726,#edf1ef);--review-muted:light-dark(#64706c,#b2bdb7);--review-accent:light-dark(#22664b,#9bd3b6);--review-soft:light-dark(#edf4ef,#2d4235);font:14px/1.6 -apple-system,BlinkMacSystemFont,sans-serif;background:var(--review-bg);color:var(--review-ink)}body{margin:0;padding:14px}button{font:inherit;color:inherit} @media(prefers-reduced-motion:reduce){*,*::before,*::after{animation:none!important;transition:none!important}}</style><script>'+__CHAPTER_SDK__+'<'+ '/script></head><body>'+state.bundle.documents[c.document]+'</body></html>';
      frame.addEventListener('load',()=>{try{frame.contentWindow.postMessage({channel:'devcroft-host-v1',chapter:c.id,evidence,reducedMotion:matchMedia('(prefers-reduced-motion:reduce)').matches},'*');}catch{}});
      canvas.append(frame);
    }else canvas.append(make('p',c.summary,'notice'));
    $('text-mode').textContent=textMode?'Show visual explanation':'Read text equivalent';
    $('claims').replaceChildren();c.claims.forEach(claim=>{const box=make('div',undefined,'claim');box.append(make('p',claim.text));claim.evidence_ids.forEach(id=>box.append(button(evidenceLabel(id),()=>{pinned=true;showEvidence(id);},'citation')));$('claims').append(box);});
    $('questions').replaceChildren();c.questions.forEach(q=>$('questions').append(button(q,()=>openQuestion(q),'question-link text-button')));
    $('evidence-links').replaceChildren();c.evidence_ids.forEach(id=>$('evidence-links').append(button(evidenceLabel(id),()=>{pinned=true;showEvidence(id);},'citation')));
    $('examined').textContent=state.examined.includes(c.id)?'Reopen behavior':'Mark examined';$('decision').textContent=state.examined.includes(c.id)?'Examined by you':'Waiting for your judgment';
    if(evidence)showEvidence(evidence);
  }
  function providerState(){const eligible=state.providers.some(p=>p.id===$('provider').value&&p.eligible);$('generate').disabled=state.busy||!eligible;$('send-question').disabled=state.busy||!eligible;}
  function evidenceLabel(id){const e=state.capture.evidence.find(e=>e.id===id);return e?`${e.path} · ${e.side} ${e.start}–${e.end}`:'Unavailable reference';}
  function code(lines,selected=null){const out=make('div',undefined,'code');lines.forEach(l=>{const row=make('div',undefined,'line '+l.tag);if(selected&&((selected.side==='old'?l.old:l.new)>=selected.start&&(selected.side==='old'?l.old:l.new)<=selected.end))row.classList.add('selected');row.append(make('span',l.old??'','number'),make('span',l.new??'','number'),make('span',l.tag==='addition'?'+':l.tag==='deletion'?'−':' '),make('span',l.text));out.append(row);});return out;}
  async function showEvidence(id){
    if(!id)return;const e=state.capture.evidence.find(e=>e.id===id);if(!e)return;
    const request=++sourceRequest;
    evidence=id;$('follow').textContent=pinned?'Follow story':'Following story';
    $('evidence-content').replaceChildren(make('h3',e.path),make('p',`${e.side==='new'?'Captured current':'Captured base'} source · lines ${e.start}–${e.end} · ${e.source_hash.slice(0,12)}`,'muted'));
    try{
      const result=await call('source',{evidence:id});if(evidence!==id||request!==sourceRequest)return;
      const lines=result.source.split('\n');if(lines.at(-1)==='')lines.pop();
      const first=whole?0:Math.max(0,e.start-1),last=whole?lines.length:e.end;
      const selected=lines.slice(first,last).map((text,i)=>({old:e.side==='old'?first+i+1:null,new:e.side==='new'?first+i+1:null,tag:'context',text}));
      $('evidence-content').replaceChildren(make('h3',e.path),make('p',`${e.side==='new'?'Captured current':'Captured base'} source · lines ${e.start}–${e.end} · ${e.source_hash.slice(0,12)}`,'muted'),code(selected,e));$('context').textContent=whole?'Show cited range':'Show surrounding source';
      $('evidence-content').append(make('p','Cited source · execution not recorded','muted'));
    }catch(e){error(e);}
  }
  function renderFiles(){
    $('file-list').replaceChildren();state.capture.files.forEach(f=>{const b=button(`${f.path}  +${f.additions} −${f.deletions}`,()=>{file=f.path;pinned=true;renderFile();},'file-button');b.setAttribute('aria-pressed',String(file===f.path));$('file-list').append(b);});
    if(!file)file=state.capture.files[0]?.path;renderFile();
  }
  function renderFile(){
    const f=state.capture.files.find(f=>f.path===file);const target=$('file-content');target.replaceChildren();if(!f){target.append(make('p','No captured changes.','notice'));return;}
    target.append(make('h3',f.path),make('p',`${f.status}${f.old_path?' · previously '+f.old_path:''} · captured unified diff`,'muted'));
    if(f.unavailable)target.append(make('p',f.unavailable,'notice'));else if(f.lines.length)target.append(code(f.lines));else target.append(make('p','Metadata-only change; no text hunk.','notice'));
    if(f.truncated)target.append(make('p','Diff excerpt truncated. The captured source remains available below.','notice'));
    const refs=state.capture.evidence.filter(e=>e.path===f.path);
    refs.forEach(e=>target.append(button(`${e.side} ${e.start}–${e.end}`,async()=>{evidence=e.id;pinned=true;const r=await call('source',{evidence:e.id}).catch(error);if(!r)return;const excerpt=make('div');excerpt.append(make('p',`${e.side} captured source · execution not recorded`,'muted'),code(r.source.replace(/\n$/,'').split('\n').map((text,i)=>({old:e.side==='old'?i+1:null,new:e.side==='new'?i+1:null,tag:'context',text})),e),button('Record a finding here',()=>openFinding({chapter:null,evidence:e.id})),button('Ask about this source',()=>openQuestion('',{chapter:null,evidence:e.id})));target.replaceChildren(make('h3',f.path),excerpt);},'citation')));
    target.append(button('Record a file concern',()=>openFinding({chapter:null,evidence:refs.find(e=>e.side==='new')?.id||refs[0]?.id||null})),button('Ask about this file',()=>openQuestion('',{chapter:null,evidence:refs[0]?.id||null})));
  }
  function renderFindings(){
    $('summary-progress').textContent=`${state.examined.length} of ${state.bundle?.manifest.chapters.length||0} behaviors examined · ${state.findings.filter(f=>!f.resolved).length} open findings`;
    $('finding-list').replaceChildren();state.findings.forEach(f=>{const box=make('div',undefined,'finding');box.append(make('h3',f.resolved?'Resolved finding':'Open finding'),make('p',f.body),make('p',f.capture===state.capture.id?'Current captured revision':'Earlier revision · needs another look','muted'));if(f.evidence&&f.capture===state.capture.id)box.append(button(evidenceLabel(f.evidence),()=>{const e=state.capture.evidence.find(e=>e.id===f.evidence);file=e?.path;page('changes');renderFile();document.querySelector('.tabs [data-page="changes"]').focus();},'text-button'));box.append(button(f.resolved?'Reopen finding':'Resolve finding',()=>call('resolve',{id:f.id,resolved:!f.resolved}).catch(error)));$('finding-list').append(box);});
    if(!state.findings.length)$('finding-list').append(make('p','No findings yet. Questions and uncertainty can remain open.','notice'));
    $('investigation-list').replaceChildren();state.investigations.forEach(q=>{const box=make('div',undefined,'finding');box.append(make('h3',q.question),make('p',q.answer||q.error||'Answer pending'),make('p',q.capture===state.capture.id?'Captured current revision':'Earlier captured revision','muted'));$('investigation-list').append(box);});
    $('history').replaceChildren(make('h3','Captured revision history'));state.history.forEach(h=>$('history').append(button(`${h.id.slice(0,10)} · HEAD ${h.head.slice(0,12)} · ${h.examined} decisions${h.id===state.capture.id?' · viewing':''}`,()=>call('revision',{id:h.id}).catch(error),'chapter-button')));
  }
  function draftKey(a,kind){return `${state.capture.id}:${kind}:${a.chapter||'source'}:${a.evidence||'none'}`;}
  function openFinding(a=anchor()){
    findingAnchor={...a};$('composer-anchor').textContent=a.evidence?evidenceLabel(a.evidence):'Captured change · general finding';
    $('finding-body').value=state.drafts[draftKey(a,'finding')]||'';$('composer-status').textContent='';$('composer').showModal();$('finding-body').focus();
  }
  function saveDraft(kind,a,value){clearTimeout(draftTimer);draftTimer=setTimeout(()=>call('draft',{key:draftKey(a,kind),body:value}).catch(e=>{if(kind==='finding')$('composer-status').textContent=e.message;error(e);}),300);}
  function openQuestion(question='',a=anchor()){
    questionAnchor={...a};$('question-anchor').textContent=a.evidence?evidenceLabel(a.evidence):'Captured behavior';$('question-body').value=question||state.drafts[draftKey(a,'question')]||'';renderThread();$('question-dialog').showModal();$('question-body').focus();
  }
  function renderThread(){
    $('thread').replaceChildren();state.investigations.filter(q=>q.capture===state.capture.id&&q.chapter===questionAnchor?.chapter&&q.evidence===questionAnchor?.evidence).forEach(q=>{$('thread').append(make('h3',q.question),make('p',q.answer||q.error||'Working on the captured source…'));});
  }
  // Authored frames can select registered evidence or open trusted UI. They
  // cannot submit a question, save a finding, or change a review decision.
  let lastFrameRequest=0;
  window.addEventListener('message',event=>{
    if(!frame||event.source!==frame.contentWindow||event.data?.channel!=='devcroft-chapter-v1')return;
    const c=active(),m=event.data;if(!c||typeof m.id!=='string'||m.id.length>100)return;
    const now=performance.now();if(now-lastFrameRequest<120)return;lastFrameRequest=now;
    if(m.type==='evidence'&&c.evidence_ids.includes(m.id)){if(!pinned)showEvidence(m.id);}
    if(m.type==='claim'){const claim=c.claims.find(x=>x.id===m.id);if(claim&&!pinned)showEvidence(claim.evidence_ids[0]);}
    if(m.type==='question'&&c.claims.some(x=>x.id===m.id))openQuestion(c.claims.find(x=>x.id===m.id).text);
    if(m.type==='finding'&&c.evidence_ids.includes(m.id))openFinding({chapter:c.id,evidence:m.id});
  });
  document.querySelectorAll('[data-page]').forEach(b=>b.addEventListener('click',()=>page(b.dataset.page)));
  $('summary-button').onclick=()=>page('findings');$('unassigned').onclick=()=>page('changes');
  $('text-mode').onclick=()=>{textMode=!textMode;renderChapter();};$('follow').onclick=()=>{pinned=false;showEvidence(active()?.evidence_ids[0]);};
  $('context').onclick=()=>{whole=!whole;pinned=true;showEvidence(evidence);};
  $('concern').onclick=()=>openFinding();$('add-finding').onclick=()=>openFinding();
  $('challenge').onclick=()=>openQuestion('What assumptions could make this behavior incorrect?');$('ask-evidence').onclick=()=>openQuestion();
  $('examined').onclick=()=>call('examined',{chapter,examined:!state.examined.includes(chapter)}).catch(error);
  $('provider').onchange=providerState;
  $('generate').onclick=()=>call('generate',{provider:$('provider').value,priorities:$('priorities').value}).catch(error);
  $('stop').onclick=()=>call('stop').catch(error);$('diagnostics').onclick=()=>$('log').hidden=!$('log').hidden;
  $('finding-body').oninput=()=>saveDraft('finding',findingAnchor,$('finding-body').value);
  $('question-body').oninput=()=>saveDraft('question',questionAnchor,$('question-body').value);
  async function closeComposer(kind){
    clearTimeout(draftTimer);
    const a=kind==='finding'?findingAnchor:questionAnchor;
    const dialog=kind==='finding'?$('composer'):$('question-dialog');
    const input=kind==='finding'?$('finding-body'):$('question-body');
    try{await call('draft',{key:draftKey(a,kind),body:input.value});dialog.close();}
    catch(e){if(kind==='finding')$('composer-status').textContent=e.message+' Use Reload saved state, then retry.';error(e);}
  }
  for(const [id,kind] of [['composer','finding'],['question-dialog','question']]){
    $(id).addEventListener('cancel',event=>{event.preventDefault();closeComposer(kind);});
    $(id).querySelectorAll('button[value="cancel"]').forEach(b=>b.onclick=event=>{event.preventDefault();closeComposer(kind);});
  }
  $('save-finding').onclick=async()=>{const body=$('finding-body').value.trim();if(!body){$('composer-status').textContent='Describe your concern before saving.';return;}clearTimeout(draftTimer);try{await call('finding',{...findingAnchor,body,key:draftKey(findingAnchor,'finding')});$('finding-body').value='';$('composer').close();}catch(e){$('composer-status').textContent=e.message;}};
  $('send-question').onclick=async()=>{const question=$('question-body').value.trim();if(!question)return;clearTimeout(draftTimer);try{await call('ask',{...questionAnchor,question,provider:$('provider').value});$('question-body').value='';renderThread();}catch(e){error(e);}};
  $('copy-summary').onclick=()=>call('copy').catch(error);
  call('ready').catch(error);
})();
