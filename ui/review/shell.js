(() => {
  'use strict';
  // The capability is delivered only in this trusted top document, never
  // through Wry initialization scripts or chapter messages.
  const token=__CAPABILITY__;
  const $=id=>document.getElementById(id);
  const pending=new Map();let sequence=0,queue=Promise.resolve(),state=null;
  let chapter=null,evidence=null,file=null,pinned=false,whole=false,textMode=false,automaticTextFallback=false,frame=null;
  let findingAnchor=null,questionAnchor=null,draftTimer=null,sourceRequest=0,localPage=null,publicationPreview=null,publicationDirty=false;const visualFailures=new Map();
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
      if(state&&state.capture.id!==message.state.capture.id){chapter=null;evidence=null;file=null;pinned=false;whole=false;localPage=null;frame=null;stamps.clear();publicationPreview=null;if($('composer').open||$('question-dialog').open)error('Viewed revision changed. Composer text belongs to the earlier capture; reopen that capture to save it.');}
      if(state&&state.bundleHash!==message.state.bundleHash){
        const chapters=message.state.bundle?.manifest.chapters||[];
        chapter=chapters.some(c=>c.id===message.state.chapter)?message.state.chapter:chapters.some(c=>c.id===chapter)?chapter:chapters[0]?.id||null;
        const selected=chapters.find(c=>c.id===chapter);
        evidence=selected?.evidence_ids.includes(message.state.evidence)?message.state.evidence:selected?.evidence_ids.includes(evidence)?evidence:selected?.evidence_ids[0]||null;
        pinned=false;whole=false;
        if(automaticTextFallback){textMode=false;automaticTextFallback=false;}
        visualFailures.clear();
      }
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
    if(active())renderChapter();
    call('location',{page:name,chapter,evidence}).then(()=>{if(localPage===name)localPage=null;}).catch(error);
  }
  function selectChapter(id){
    if(!state.bundle?.manifest.chapters.some(c=>c.id===id))return;
    chapter=id;textMode=false;automaticTextFallback=false;pinned=false;whole=false;evidence=active().evidence_ids[0]||null;
    renderChapter();renderOutline();providerState();page('walkthrough');
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
    if(!$('review-body').dataset.capture||$('review-body').dataset.capture!==state.capture.id){$('review-body').value=state.drafts[draftKey({},'publication')]||'';$('review-body').dataset.capture=state.capture.id;}
    $('overview-summary').textContent=state.bundle?.manifest.summary||'Inspect all captured changes now. Guide generation is optional and never blocks source review.';
    $('summary-button').textContent=`Review findings${state.findings.length?' ('+state.findings.length+')':''}`;
    const preferred=$('provider').value||state.provider;
    const selected=state.providers.find(p=>p.id===preferred&&p.eligible)?.id||state.providers.find(p=>p.eligible)?.id||preferred;
    if(changed('providers',state.providers)){$('provider').replaceChildren();state.providers.forEach(p=>{const option=make('option',p.label);option.value=p.id;option.disabled=!p.eligible;$('provider').append(option);});$('provider').value=selected;}
    $('sharing').textContent=state.sharing;
    $('question-sharing').textContent=state.sharing;
    $('stop').hidden=!state.busy;
    if(!chapter||!active())chapter=state.chapter||state.bundle?.manifest.chapters[0]?.id||null;
    if(!evidence)evidence=state.evidence||active()?.evidence_ids[0]||null;
    providerState();
    if(changed('outline',[state.bundleHash,state.examined,chapter]))renderOutline();
    if(changed('findings',[state.findings,state.investigations,state.history,state.examined,state.busy]))renderFindings();
    if(changed('publication',[state.submissions,state.publishing,state.capturing,state.preview,state.version]))renderPublication();
    if(changed('guides',[state.guideHistory,state.bundleHash])){const target=$('guide-history');target.replaceChildren();if(state.guideHistory?.length){target.append(make('h3','Earlier explanations for this capture'));state.guideHistory.forEach(g=>target.append(button(`${g.title} · ${g.examined} decisions · ${g.hash.slice(0,10)}`,()=>call('guide',{hash:g.hash}).catch(error),'chapter-button')));}}
    if(changed('pr',state.capture.pr))renderPr();
    if(changed('files',state.capture.id))renderFiles();
    // Keep the mounted canvas and its presentation state during saves.
    if($('chapter-canvas').dataset.chapter!==chapter||$('chapter-canvas').dataset.bundle!==state.bundleHash||$('chapter-canvas').dataset.page!==(localPage||state.page))renderChapter();
    const c=active();$('examined').textContent=state.examined.includes(chapter)?'Reopen behavior':'Mark examined';
    $('decision').textContent=state.examined.includes(chapter)?'Examined by you':'Waiting for your judgment';
    let current=localPage||state.page||'changes';if(current==='walkthrough'&&!c)current='changes';
    for(const p of ['overview','walkthrough','findings','changes'])$(p).hidden=p!==current;
    document.querySelectorAll('.tabs [data-page]').forEach(b=>b.setAttribute('aria-pressed',String(b.dataset.page===current)));
    if($('question-dialog').open)renderThread();
  }
  function renderChapter(){
    const c=active();if(!c){const canvas=$('chapter-canvas');canvas.replaceChildren();delete canvas.dataset.chapter;delete canvas.dataset.bundle;delete canvas.dataset.page;frame=null;return;}
    $('chapter-label').textContent='Behavior · captured interpretation';$('chapter-title').textContent=c.title;$('chapter-summary').textContent=c.summary;
    const canvas=$('chapter-canvas');canvas.replaceChildren();frame=null;canvas.dataset.chapter=c.id;canvas.dataset.bundle=state.bundleHash;canvas.dataset.page=localPage||state.page;
    if(!textMode&&(localPage||state.page)==='walkthrough'){
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
  function providerState(){const chosen=state.providers.find(p=>p.id===$('provider').value);if($('author-status'))$('author-status').textContent=chosen?.reason||(!chosen?.eligible?'This provider’s confined authoring adapter is unavailable.':'');const eligible=state.providers.some(p=>p.id===$('provider').value&&p.eligible);$('generate').disabled=state.busy||state.publishing||state.capturing||!eligible;$('repair-visual').disabled=state.busy||state.publishing||state.capturing||!eligible||!active();$('send-question').disabled=state.busy||state.publishing||state.capturing||!eligible;}
  function evidenceLabel(id){const e=state.capture.evidence.find(e=>e.id===id);return e?`${e.path} · ${e.side} ${e.start}–${e.end}`:'Unavailable reference';}
  function code(lines,selected=null,path=null){
    const out=make('div',undefined,'code');let offset=0;
    const more=button('Show more captured lines',append);const progress=make('p',undefined,'muted');
    function number(n,side){
      if(n===null||n===undefined||!path)return make('span',n??'','number');
      const b=button(n,()=>{
        const e=state.capture.evidence.find(e=>e.path===path&&e.side===side&&e.start<=n&&e.end>=n);
        if(e)openFinding({chapter:active()?.evidence_ids.includes(e.id)?chapter:null,evidence:e.id,range:{start:n,end:n}});
      },'number');b.setAttribute('aria-label',`Record finding on ${side} line ${n}`);return b;
    }
    function append(){
      more.remove();progress.remove();
      lines.slice(offset,offset+500).forEach(l=>{const row=make('div',undefined,'line '+l.tag);if(selected&&((selected.side==='old'?l.old:l.new)>=selected.start&&(selected.side==='old'?l.old:l.new)<=selected.end))row.classList.add('selected');row.append(number(l.old,'old'),number(l.new,'new'),make('span',l.tag==='addition'?'+':l.tag==='deletion'?'−':' '),make('span',l.text));out.append(row);});
      offset=Math.min(lines.length,offset+500);
      if(offset<lines.length){progress.textContent=`Showing ${offset} of ${lines.length} captured lines`;out.append(progress,more);}
    }
    append();return out;
  }
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
      $('evidence-content').replaceChildren(make('h3',e.path),make('p',`${e.side==='new'?'Captured current':'Captured base'} source · lines ${e.start}–${e.end} · ${e.source_hash.slice(0,12)}`,'muted'),code(selected,e,e.path));$('context').textContent=whole?'Show cited range':'Show surrounding source';
      $('evidence-content').append(make('p','Cited source · execution not recorded','muted'));
    }catch(e){error(e);}
  }
  function renderFiles(){
    $('file-list').replaceChildren();state.capture.files.forEach(f=>{const b=button(`${f.path}${f.unavailable?' · source unavailable':'  +'+f.additions+' −'+f.deletions}`,()=>{file=f.path;pinned=true;renderFile();},'file-button');b.setAttribute('aria-pressed',String(file===f.path));$('file-list').append(b);});
    if(!file)file=state.capture.files[0]?.path;renderFile();
  }
  function renderFile(){
    const f=state.capture.files.find(f=>f.path===file);const target=$('file-content');target.replaceChildren();if(!f){target.append(make('p','No captured changes.','notice'));return;}
    target.append(make('h3',f.path),make('p',`${f.status}${f.old_path?' · previously '+f.old_path:''} · captured unified diff`,'muted'));
    if(f.unavailable)target.append(make('p',f.unavailable,'notice'));else if(f.lines.length)target.append(code(f.lines,null,f.path));else target.append(make('p','Metadata-only change; no text hunk.','notice'));
    if(f.truncated)target.append(make('p','Diff excerpt truncated. The captured source remains available below.','notice'));
    const refs=state.capture.evidence.filter(e=>e.path===f.path);
    refs.forEach(e=>target.append(button(`${e.side} ${e.start}–${e.end}`,async()=>{evidence=e.id;pinned=true;const r=await call('source',{evidence:e.id}).catch(error);if(!r)return;const excerpt=make('div');excerpt.append(make('p',`${e.side} captured source · execution not recorded`,'muted'),code(r.source.replace(/\n$/,'').split('\n').map((text,i)=>({old:e.side==='old'?i+1:null,new:e.side==='new'?i+1:null,tag:'context',text})),e,f.path),button('Record a finding here',()=>openFinding({chapter:null,evidence:e.id})),button('Ask about this source',()=>openQuestion('',{chapter:null,evidence:e.id})));target.replaceChildren(make('h3',f.path),excerpt);},'citation')));
    target.append(button('Record a file concern',()=>openFinding({chapter:null,evidence:refs.find(e=>e.side==='new')?.id||refs[0]?.id||null})),button('Ask about this file',()=>openQuestion('',{chapter:null,evidence:refs[0]?.id||null})));
  }
  function renderFindings(){
    $('summary-progress').textContent=`${state.examined.length} of ${state.bundle?.manifest.chapters.length||0} behaviors examined · ${state.findings.filter(f=>!f.resolved).length} open findings`;
    $('finding-list').replaceChildren();state.findings.forEach(f=>{const box=make('div',undefined,'finding');box.append(make('h3',f.resolved?'Resolved finding':'Open finding'),make('p',f.body),make('p',f.capture===state.capture.id?'Current captured revision':'Earlier revision · needs another look','muted'));if(f.evidence&&f.capture===state.capture.id)box.append(button(evidenceLabel(f.evidence),()=>{const e=state.capture.evidence.find(e=>e.id===f.evidence);file=e?.path;page('changes');renderFile();document.querySelector('.tabs [data-page="changes"]').focus();},'text-button'));box.append(button(f.resolved?'Reopen finding':'Resolve finding',()=>call('resolve',{id:f.id,resolved:!f.resolved}).catch(error)));$('finding-list').append(box);});
    if(!state.findings.length)$('finding-list').append(make('p','No findings yet. Questions and uncertainty can remain open.','notice'));
    $('investigation-list').replaceChildren();state.investigations.forEach(q=>{const box=make('div',undefined,'finding');box.append(make('h3',q.question),make('p',q.answer||q.error||'Answer pending'),make('p',q.capture===state.capture.id?'Captured current revision':'Earlier captured revision','muted'));$('investigation-list').append(box);});
    $('history').replaceChildren(make('h3','Captured revision history'));state.history.forEach(h=>$('history').append(button(`${h.id.slice(0,10)} · HEAD ${h.head.slice(0,12)} · ${h.examined} decisions${h.id===state.capture.id?' · viewing':''}`,()=>call('revision',{id:h.id}).catch(error),'chapter-button')));
  }
  function draftKey(a,kind){return `${a.capture||state.capture.id}:${kind}:${a.chapter||'source'}:${a.evidence||'none'}`;}
  function openFinding(a=anchor()){
    findingAnchor={...a,capture:state.capture.id};const e=state.capture.evidence.find(e=>e.id===a.evidence);$('finding-range').hidden=!e;$('attach-lines').checked=!!a.range;for(const id of ['range-start','range-end']){const input=$(id);input.min=e?.start||1;input.max=e?.end||1;}$('range-start').value=a.range?.start||e?.start||1;$('range-end').value=a.range?.end||e?.end||1;$('composer-anchor').textContent=a.evidence?evidenceLabel(a.evidence):'Captured change · general finding';
    $('finding-body').value=state.drafts[draftKey(a,'finding')]||'';$('composer-status').textContent='';$('composer').showModal();$('finding-body').focus();
  }
  function saveDraft(kind,a,value){clearTimeout(draftTimer);draftTimer=setTimeout(()=>call('draft',{key:draftKey(a,kind),body:value}).catch(e=>{if(kind==='finding')$('composer-status').textContent=e.message;error(e);}),300);}
  function openQuestion(question='',a=anchor()){
    questionAnchor={...a,capture:state.capture.id};$('question-anchor').textContent=a.evidence?evidenceLabel(a.evidence):'Captured behavior';$('question-body').value=question||state.drafts[draftKey(a,'question')]||'';renderThread();$('question-dialog').showModal();$('question-body').focus();
  }
  function renderThread(){
    $('thread').replaceChildren();state.investigations.filter(q=>q.capture===state.capture.id&&q.chapter===questionAnchor?.chapter&&q.evidence===questionAnchor?.evidence).forEach(q=>{$('thread').append(make('h3',q.question),make('p',q.answer||q.error||(state.busy?'Working on the captured source…':'No answer was saved. Ask again to continue this investigation.')));});
  }
  // Authored frames can select registered evidence or open trusted UI. They
  // cannot submit a question, save a finding, or change a review decision.
  let lastFrameRequest=0;
  window.addEventListener('message',event=>{
    if(!frame||event.source!==frame.contentWindow||event.data?.channel!=='devcroft-chapter-v1')return;
    const c=active(),m=event.data;if(!c||typeof m.id!=='string')return;
    if(m.type==='error'&&m.id.length<=600){visualFailures.set(c.id,m.id);textMode=true;automaticTextFallback=true;renderChapter();error('Visual explanation failed. The text equivalent and captured source remain available. '+m.id);return;}
    if(m.id.length>100)return;
    const now=performance.now();if(now-lastFrameRequest<120)return;lastFrameRequest=now;
    if(m.type==='evidence'&&c.evidence_ids.includes(m.id)){if(!pinned)showEvidence(m.id);}
    if(m.type==='claim'){const claim=c.claims.find(x=>x.id===m.id);if(claim&&!pinned)showEvidence(claim.evidence_ids[0]);}
    if(m.type==='question'&&c.claims.some(x=>x.id===m.id))openQuestion(c.claims.find(x=>x.id===m.id).text);
    if(m.type==='finding'&&c.evidence_ids.includes(m.id))openFinding({chapter:c.id,evidence:m.id});
  });
  document.querySelectorAll('[data-page]').forEach(b=>b.addEventListener('click',()=>page(b.dataset.page)));
  $('summary-button').onclick=()=>page('findings');$('unassigned').onclick=()=>page('changes');
  $('repair-visual').onclick=()=>call('repair',{provider:$('provider').value,chapter,problem:visualFailures.get(chapter)||'Improve readability and accessible interaction while keeping the existing explanation and evidence.'}).catch(error);
  $('text-mode').onclick=()=>{textMode=!textMode;automaticTextFallback=false;renderChapter();};$('follow').onclick=()=>{pinned=false;showEvidence(active()?.evidence_ids[0]);};
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
    if(a.capture!==state.capture.id){error('Reopen the composer’s captured revision before saving its draft.');return;}
    const dialog=kind==='finding'?$('composer'):$('question-dialog');
    const input=kind==='finding'?$('finding-body'):$('question-body');
    try{await call('draft',{key:draftKey(a,kind),body:input.value});dialog.close();}
    catch(e){if(kind==='finding')$('composer-status').textContent=e.message+' Use Reload saved state, then retry.';error(e);}
  }
  for(const [id,kind] of [['composer','finding'],['question-dialog','question']]){
    $(id).addEventListener('cancel',event=>{event.preventDefault();closeComposer(kind);});
    $(id).querySelectorAll('button[value="cancel"]').forEach(b=>b.onclick=event=>{event.preventDefault();closeComposer(kind);});
  }
  $('save-finding').onclick=async()=>{const body=$('finding-body').value.trim();if(!body){$('composer-status').textContent='Describe your concern before saving.';return;}clearTimeout(draftTimer);try{if(findingAnchor.capture!==state.capture.id)throw new Error('Reopen the finding’s captured revision before saving.');const range=$('attach-lines').checked?{start:Number($('range-start').value),end:Number($('range-end').value)}:null;await call('finding',{...findingAnchor,range,body,key:draftKey(findingAnchor,'finding')});$('finding-body').value='';$('composer').close();}catch(e){$('composer-status').textContent=e.message;}};
  $('send-question').onclick=async()=>{const question=$('question-body').value.trim();if(!question)return;clearTimeout(draftTimer);try{if(questionAnchor.capture!==state.capture.id)throw new Error('Reopen the question’s captured revision before sending.');await call('ask',{...questionAnchor,question,provider:$('provider').value});$('question-body').value='';renderThread();}catch(e){error(e);}};
  function renderPr(){
    const p=state.capture.pr,target=$('pr-context');target.replaceChildren();target.hidden=!p;if(!p)return;
    target.append(make('h3',`${p.repository} #${p.number} · ${p.title}`),make('p',p.description||'No PR description supplied.'),make('p',`Author ${p.author} · ${p.head_branch} → ${p.base_branch} · target tip ${p.target_tip.slice(0,12)} · captured ${new Date(p.captured_at*1000).toLocaleString()}`,'muted'),make('h3','CI observations'));
    if(p.checks_error)target.append(make('p',p.checks_error,'notice'));
    p.checks.forEach(c=>target.append(make('p',`${c.name}: ${c.conclusion||c.status||'unknown'} · SHA ${c.head_sha?.slice(0,12)||'unknown'}${c.head_sha!==state.capture.head?' · different revision':''} · completed ${c.completed_at||'not recorded'}`,'muted')));
    if(!p.checks.length&&!p.checks_error)target.append(make('p','No check runs recorded for this head. Test source is not an execution result.','muted'));
  }
  function openPreview(preview,saved=false){
    publicationPreview={...preview,saved};publicationDirty=false;$('preview-identity').textContent=`${state.capture.pr?.repository} · ${preview.event} · head ${preview.payload.commit_id} · payload ${preview.hash}`;
    $('preview-content').replaceChildren(make('h3','Review body'),make('pre',preview.payload.body));
    preview.payload.comments.forEach(c=>{const box=make('div',undefined,'finding');box.append(make('h3',`${c.path} · ${c.side} ${c.start_line||c.line}–${c.line}`),make('pre',c.body));$('preview-content').append(box);});
    $('preview-status').textContent=saved?'This is the saved publication intent. GitHub will be reconciled before continuing.':'Only the content shown here will be sent. Further edits require a new preview.';
    $('publish-review').textContent=saved?'Reconcile and submit this saved review':'Publish this review to GitHub';$('publish-review').disabled=state.publishing||state.capturing||(!saved&&preview.version!==state.version);
    renderPublication();if(!$('publication-dialog').open)$('publication-dialog').showModal();
  }
  function renderPublication(){
    const pr=state.capture.pr;$('publication-controls').hidden=!pr;$('publication-scope').textContent=pr?'Findings are local until you preview and explicitly publish a GitHub review. Only unresolved findings for the viewed revision are included.':'Findings are saved locally. Remote publication requires a captured GitHub PR.';
    $('preview-review').disabled=state.publishing||state.capturing||state.busy;
    $('publication-history').replaceChildren();
    state.submissions?.forEach(s=>{const box=make('div',undefined,'finding');box.append(make('h3',`GitHub review · ${s.state}`),make('p',`${s.preview.event} · head ${s.preview.payload.commit_id.slice(0,12)} · payload ${s.preview.hash.slice(0,12)}`,'muted'));if(s.error)box.append(make('p',s.error));if(s.remote_url)box.append(make('p',s.remote_url,'muted'));if(!['submitted','rejected','discarded'].includes(s.state)){const inspect=button('Inspect saved preview',()=>openPreview(s.preview,true));const reconcile=button('Check GitHub status',()=>call('reconcile',{hash:s.preview.hash}).catch(error));inspect.disabled=reconcile.disabled=state.publishing||state.capturing;box.append(inspect,reconcile);}$('publication-history').append(box);});
    if(publicationPreview){const saved=state.submissions?.find(s=>s.preview.hash===publicationPreview.hash);const stale=!publicationPreview.saved&&!saved&&publicationPreview.version!==state.version;const submitted=saved?.state==='submitted',rejected=saved?.state==='rejected',discarded=saved?.state==='discarded',discarding=saved?.state==='discarding',dirty=publicationDirty&&!publicationPreview.saved; $('publish-review').disabled=state.publishing||state.capturing||stale||submitted||rejected||discarded||discarding||dirty;$('discard-review').hidden=!saved?.remote_id||submitted||rejected||discarded;$('discard-review').disabled=state.publishing||state.capturing||state.busy;$('preview-status').textContent=state.publishing?'Checking and saving GitHub review status…':submitted?'This review was submitted to GitHub.':rejected?'GitHub rejected creation. Correct the review and inspect a new preview.':discarded?'This pending GitHub draft was removed. Local findings remain saved. Capture the updated PR and inspect a fresh preview.':discarding?'Draft removal needs reconciliation. Retry deleting this exact draft; a new review remains blocked.':saved?.remote_id&&!submitted?(saved.error?saved.error+' ':'')+'This saved review has a known GitHub draft. Publish only if the PR still matches; deleting this pending draft removes its GitHub comments and keeps local findings.':dirty?'Summary edited. Inspect a new preview before publishing.':stale?'The review changed after this preview. Close it and inspect a new preview.':saved?.error||'Exact preview ready for your decision.';}
  }
  $('preview-review').onclick=async()=>{clearTimeout(draftTimer);try{await call('draft',{key:draftKey({},'publication'),body:$('review-body').value});const preview=await call('preview',{event:$('review-event').value,body:$('review-body').value});openPreview(preview);}catch(e){error(e);}};
  $('review-body').oninput=()=>{publicationDirty=true;if(publicationPreview){$('publish-review').disabled=true;$('preview-status').textContent='Summary edited. Inspect a new preview before publishing.';}saveDraft('publication',{},$('review-body').value);};
  $('review-event').onchange=()=>{publicationPreview=null;$('publish-review').disabled=true;};
  $('close-publication').onclick=()=>$('publication-dialog').close();
  $('publish-review').onclick=async()=>{if(!publicationPreview)return;try{await call('publish',{hash:publicationPreview.hash});renderPublication();}catch(e){$('preview-status').textContent=e.message;error(e);}};
  $('discard-review').onclick=async()=>{if(!publicationPreview)return;try{await call('discard',{hash:publicationPreview.hash});renderPublication();}catch(e){$('preview-status').textContent=e.message;error(e);}};
  $('copy-summary').onclick=()=>call('copy').catch(error);
  call('ready').catch(error);
})();
