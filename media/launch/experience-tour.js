/* Fixed-time illustrative film. No network, provider calls or user data. */
window.PIKA_FILM = {seconds:60, stills:[.2,1,2,3.5,3.82,3.94,4.2,7,11,16,21,25,28,31,35,41,45,50,54,58], basename:'pika-experience.mp4'};
const cuts=[0,4,10,24,33,39,44,57,60];
const scenes=[
()=>`<div class="intro"><div class="eyebrow">Put previous work to use</div><h1>You already solved this<br><span class="accent intro-hook">somewhere else.</span></h1><p class="definition">Pika: a terminal board and skill<br>for your existing coding agents.</p><div class="providers"><span>Codex</span><span>Claude Code</span><span>OpenCode</span></div><div class="intro-network" aria-label="Existing conversations connected through Pika"><div class="intro-node" id="intro-caller"><div class="label">strategy_dashboard</div><p>Performance charts</p></div><div class="intro-link"><div class="intro-wire" id="intro-wire"></div><span>PIKA</span><i id="intro-signal"></i></div><div class="intro-node" id="intro-expert"><div class="label">returns_tracker</div><p>Daily returns expertise</p></div></div></div>`,
()=>`<div class="discovery"><h1>Your agent finds<br><span class="accent">relevant experience.</span></h1><div class="row"><div class="panel task"><div class="label">strategy_dashboard · PERFORMANCE DASHBOARD</div><h2>Adding a chart.</h2><p>Needs the daily returns source.</p><div class="search-action"><div class="label">AGENT TOOL CALL · agent-convo</div><code>pika experts "daily returns" --json</code></div></div><div class="panel expert" id="expert"><div class="label mint">EXPERT CARD</div><h2>returns_tracker</h2><p>Daily returns pipeline</p><div class="code mint">Published data · schema</div></div></div><div class="context-ribbon">Pika: a terminal board and skill for your existing coding agents.<br><span class="muted">Codex · Claude Code · OpenCode</span></div></div>`,
()=>`<div class="consult"><h1>Borrow its experience.<br><span class="accent">Let it keep working.</span></h1><div class="row"><div class="caller"><div class="caller-head"><h3>strategy_dashboard</h3><span>Performance dashboard</span></div><div class="connector"></div><div class="side" id="side"><div class="label">Separate context: returns_tracker</div><div class="ask-command">pika ask &lt;exact-id&gt; --jsonl</div><div class="question">“Which dataset should this chart use?”</div><div class="answer" id="answer">“Use the published daily returns—<br>not a second calculation.”</div><div class="side-close" id="side-close">Side closed</div></div><div class="continued" id="continued">✓ Checks schema. Connects chart.</div></div><div class="panel original" id="original"><div class="label">returns_tracker · ORIGINAL</div><h3>Checking daily data</h3><div class="code"><div class="test-row" id="test-0"><span>data timestamps</span><span></span></div><div class="test-row" id="test-1"><span>portfolio coverage</span><span></span></div><div class="test-row" id="test-2"><span>scheduled updates</span><span></span></div></div><div class="progress"><div id="work-progress"></div></div><div class="status"><i class="dot" id="work-dot"></i> Working</div></div></div><div class="guarantee" id="guarantee">No questions or replies enter the original conversation.</div></div>`,
()=>`<div><h1>See who <span class="accent">needs you.</span></h1><div class="board-command"><div class="command">pika<span class="cursor"></span></div><div class="tag">Update available · review and approve</div></div><div class="panel board"><div class="board-bar"><span>PIKA / LIVE OPERATIONS</span><span class="accent">1 needs you <span class="muted">· 2 working</span></span></div><div class="board-body"><div class="inventory"><div class="group">NEEDS YOU</div><div class="item" id="approval-row"><span>◆ release-notes</span><span class="accent">approval</span></div><div class="group">WORKING</div><div class="item"><span>□ strategy_dashboard</span><span class="blue">working</span></div><div class="item"><span>□ returns_tracker</span><span class="blue">working</span></div><div class="group">RESULT READY</div><div class="item" id="result-row"><span>◇ api-migration</span><span class="mint">ready</span></div></div><div class="detail" id="detail"></div></div><div class="board-keys"><span id="peek-key">p · peek result</span> &nbsp; <span id="unwatch-key">x · stop watching, keep history</span></div></div></div>`,
()=>`<div class="recovery"><h1>Return to<br><span class="accent">your conversation.</span></h1><div class="row"><div><div class="label">BOARD → ENTER</div><div class="command" style="margin-top:22px">pika strategy_dashboard</div><p class="caption">Or open directly by name.</p></div><div class="panel native"><div class="label mint">CODEX · EXACT CONVERSATION</div><h3>strategy_dashboard</h3><div class="code">Daily returns source checked.<br>Connecting performance chart.</div><div class="receipt">Same conversation. Back in Codex.</div><div class="status">› <span class="cursor"></span></div></div></div></div>`,
()=>`<div class="machines"><h1>Across trusted<br><span class="accent">machines, too.</span></h1><h2>One board. Cross-machine consultations.</h2><div class="routes"><span>SSH / Tailscale</span><span class="muted">·</span><span>Pika on each machine</span></div><div class="three"><div class="panel machine"><div class="label">HERE</div><h2>Your laptop</h2><div class="status">● Local agents</div></div><div class="panel machine"><div class="label">APPROVED MACHINE</div><h2>Build server</h2><div class="status">● Connected</div></div><div class="panel machine"><div class="label">APPROVED MACHINE</div><h2>Research server</h2><div class="status">● Connected</div></div></div></div>`,
()=>`<div class="setup"><h1>Start with<br><span class="accent">one conversation.</span></h1><div class="steps"><div class="panel step" id="step-0"><div class="number">01</div><div><h3>README → Install → <span class="mint">pika setup</span></h3><p>Includes agent-convo. Select one conversation.</p></div></div><div class="panel step" id="step-1"><div class="number">02</div><div><h3>Create its expert card.</h3><p>Follow the first-consultation guide.</p></div></div><div class="panel step" id="step-2"><div class="number">03</div><div><h3>Tell your agent:</h3><p class="mint">“Use agent-convo when prior work helps.”</p></div></div></div><a class="repo" href="https://github.com/ayushjainr/pikamux">github.com/ayushjainr/pikamux ↗</a></div>`,
()=>`<div class="setup"><h1>Start with<br><span class="accent">one conversation.</span></h1><div class="steps"><div class="panel step"><div class="number">01</div><div><h3>README → Install → <span class="mint">pika setup</span></h3><p>Includes agent-convo. Select one conversation.</p></div></div><div class="panel step"><div class="number">02</div><div><h3>Create its expert card.</h3><p>Follow the first-consultation guide.</p></div></div><div class="panel step"><div class="number">03</div><div><h3>Tell your agent:</h3><p class="mint">“Use agent-convo when prior work helps.”</p></div></div></div><a class="repo" href="https://github.com/ayushjainr/pikamux">github.com/ayushjainr/pikamux ↗</a><div class="byline">Mac + Linux · Open source &nbsp; · &nbsp; Ayush Jain · ayushjainr.com</div></div>`
];
const scene=document.getElementById('scene');
document.getElementById('rail').innerHTML=cuts.slice(0,-1).map(()=>'<span></span>').join('');
const ease=x=>1-(1-Math.max(0,Math.min(1,x)))**3;
const reveal=(id,p)=>{const e=document.getElementById(id);e.style.opacity=ease(p);e.style.transform=`translateY(${(1-ease(p))*10}px)`;};
let last=-1;
window.renderFrame=function(t){
 t=Math.max(0,Math.min(59.999,t));const n=cuts.findIndex((v,i)=>t>=v&&t<cuts[i+1]),local=t-cuts[n];
 if(n!==last){scene.innerHTML=scenes[n]();last=n;}
 scene.style.transform=`translateX(${n===0||n===1||n===7?0:(1-ease(local/.18))*16}px)`;
 if(n===0){
  const intro=scene.querySelector('.intro');
  for(const[selector,delay]of [['.definition',.25]]){
   const e=intro.querySelector(selector),p=ease((local-delay)/.3);
   e.style.opacity=p;e.style.transform=`translateY(${(1-p)*10}px)`;
  }
  const hook=intro.querySelector('.intro-hook');
  hook.style.clipPath=`inset(0 ${(1-ease((local-.05)/.65))*100}% 0 0)`;
  intro.querySelectorAll('.providers span').forEach((e,i)=>{
   const p=ease((local-.65-i*.16)/.3);e.style.opacity=p;e.style.transform=`translateY(${(1-p)*14}px)`;
  });
  reveal('intro-caller',(local-.8)/.4);reveal('intro-expert',(local-1.15)/.4);
  const wire=document.getElementById('intro-wire');wire.style.transform=`scaleY(${ease((local-1.5)/.5)})`;
  const signal=document.getElementById('intro-signal'),progress=Math.max(0,Math.min(1,(local-1.8)/1.1));
  signal.style.transform=`translateY(${progress*100}px)`;
  signal.style.opacity=local>=1.8&&local<2.9?1:0;
  document.getElementById('intro-expert').style.borderColor=local>=2.9?'#a1e2cd':'#494258';
  const blend=Math.max(0,Math.min(1,(t-3.76)/.24));
  // Stagger the text fades so different headlines never overprint each other.
  intro.style.opacity=1-ease(blend*2);
  let incoming=document.getElementById('intro-next');
  if(blend>0){
   if(!incoming){incoming=document.createElement('div');incoming.id='intro-next';incoming.innerHTML=scenes[1]();scene.append(incoming);}
   incoming.style.opacity=ease(blend*2-1);
   const expert=incoming.querySelector('#expert');expert.style.opacity=0;expert.style.transform='translateY(10px)';
  }else if(incoming)incoming.remove();
 }
 if(n===1)reveal('expert',(local-1.2)/.3);
 if(n===2){
  reveal('answer',(local-4)/.22);reveal('continued',(local-9)/.25);reveal('side-close',(local-10)/.22);reveal('guarantee',(local-9)/.22);
  document.getElementById('work-progress').style.width=`${18+local*5.4}%`;
  document.getElementById('work-dot').style.opacity=.55+.45*Math.sin(t*3)**2;
  for(let i=0;i<3;i++){const e=document.getElementById(`test-${i}`),done=local>2+i*4;e.classList.toggle('done',done);e.lastElementChild.textContent=done?'✓':local>i*4?'…':'·';}
 }
 if(n===3){
  const mode=local<3?0:local<6?1:2;
  document.getElementById('approval-row').classList.toggle('chosen',mode===0);
  document.getElementById('result-row').classList.toggle('chosen',mode===1);
  document.getElementById('result-row').style.visibility=mode===2?'hidden':'visible';
  document.getElementById('peek-key').className=mode===1?'active':'';
  document.getElementById('unwatch-key').className=mode===2?'active':'';
  document.getElementById('detail').innerHTML=mode===0?'<div class="label accent">NEEDS APPROVAL</div><h2>release-notes</h2><p>Your agent is waiting<br>for your decision.</p>':mode===1?'<div class="label mint">RESULT PREVIEW</div><h2>api-migration</h2><div class="code">Migration complete.<br>Compatibility checks passed.</div>':'<div class="label mint">STOPPED WATCHING</div><h2>api-migration</h2><p>History kept.<br>Stay on the board.</p>';
 }
 if(n===6)for(let i=0;i<3;i++)reveal(`step-${i}`,(local-i*4)/.22);
 document.querySelectorAll('#rail span').forEach((e,i)=>e.classList.toggle('active',i<=n));
 document.getElementById('seek').value=t;document.getElementById('time').textContent=Math.floor(t)+' / 60s';return n;
};
function resize(){document.getElementById('stage').style.transform=`translate(-50%,-50%) scale(${Math.min(innerWidth/1920,innerHeight/1080)})`;}
addEventListener('resize',resize);resize();
const params=new URLSearchParams(location.search);let position=Number(params.get('t')||0),playing=!params.has('t'),previous=performance.now();
if(params.has('render')){playing=false;document.getElementById('controls').style.display='none';}
window.renderFrame(position);
function tick(now){if(playing){position=(position+(now-previous)/1000)%60;window.renderFrame(position);}previous=now;requestAnimationFrame(tick);}requestAnimationFrame(tick);
function toggle(){playing=!playing;document.getElementById('play').textContent=playing?'Pause':'Play';}
document.getElementById('play').onclick=toggle;
document.getElementById('seek').oninput=e=>{position=Number(e.target.value);playing=false;window.renderFrame(position);};
addEventListener('keydown',e=>{if(e.code==='Space'){e.preventDefault();toggle();}if(e.code==='ArrowRight'||e.code==='ArrowLeft'){e.preventDefault();position=Math.max(0,Math.min(59.9,position+(e.code==='ArrowRight'?5:-5)));playing=false;window.renderFrame(position);}});
