const assert=require('node:assert/strict');
const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright-core');
const {pathToFileURL}=require('node:url');
const path=require('node:path');
(async()=>{
 const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_PATH});
 try{
  const page=await browser.newPage({viewport:{width:1920,height:1080}}),errors=[];
  page.on('pageerror',e=>errors.push(String(e)));
  await page.goto(pathToFileURL(path.join(__dirname,'experience-tour.html')).href+'?render=1');
  assert.equal(await page.evaluate(()=>PIKA_FILM.seconds),60);
  const samples=[[1,/terminal board and skill/],[3.88,/pika experts "daily returns" --json/],[4.2,/pika experts "daily returns" --json/],[7,/returns_tracker/],[16,/Separate context: returns_tracker/],[21,/No questions or replies/],[25,/strategy_dashboard/],[28,/RESULT PREVIEW/],[31,/History kept/],[35,/pika strategy_dashboard/],[41,/Pika on each machine/],[45,/pika setup/],[50,/Create its expert card/],[54,/Use agent-convo when prior work helps/],[58,/ayushjainr.com/]];
  for(const[t,pattern]of samples){
   await page.evaluate(t=>renderFrame(t),t);
   assert.match(await page.locator('#scene').innerText(),pattern);
   assert.doesNotMatch(await page.locator('#scene').innerText(),/support-inbox|Support inbox|CSV|export_columns|qis_dash/);
   const overflow=await page.evaluate(()=>[...document.querySelectorAll('#scene *,.edition')].flatMap(e=>{const r=e.getBoundingClientRect();return r.width&&r.height&&(r.left<85||r.right>1835||r.top<0||r.bottom>977||e.scrollWidth>e.clientWidth+3)?[`${e.className||e.tagName}: ${Math.round(r.right)},${Math.round(r.bottom)}`]:[]}));
   assert.deepEqual(overflow,[],`Layout at ${t}s`);
   const html=await page.locator('#scene').innerHTML();
   await page.evaluate(()=>renderFrame(59));await page.evaluate(t=>renderFrame(t),t);
   assert.equal(await page.locator('#scene').innerHTML(),html,`Deterministic seek at ${t}s`);
  }
  await page.evaluate(()=>renderFrame(.1));
  assert.equal(await page.locator('.definition').evaluate(e=>Number(e.style.opacity)),0);
  await page.evaluate(()=>renderFrame(1.2));
  assert.equal(await page.locator('.definition').evaluate(e=>Number(e.style.opacity)),1);
  await page.evaluate(()=>renderFrame(2));
  const signalBefore=await page.locator('#intro-signal').getAttribute('style');
  assert.equal(await page.locator('#intro-caller').evaluate(e=>Number(e.style.opacity)),1);
  await page.evaluate(()=>renderFrame(2.6));
  assert.notEqual(await page.locator('#intro-signal').getAttribute('style'),signalBefore);
  await page.evaluate(()=>renderFrame(3.5));
  assert.equal(await page.locator('#intro-expert').evaluate(e=>e.style.borderColor),'rgb(161, 226, 205)');
  for(const t of [3.82,3.94]){
   await page.evaluate(t=>renderFrame(t),t);
   const blend=await page.evaluate(()=>[Number(document.querySelector('.intro').style.opacity),Number(document.getElementById('intro-next').style.opacity)]);
   assert(blend.some(v=>v>0&&v<1));assert(blend.some(v=>v===0),'Transition must not overprint headlines');
  }
  await page.evaluate(()=>renderFrame(4));
  assert.equal(await page.locator('.intro').count(),0);
  assert.match(await page.locator('.context-ribbon').innerText(),/terminal board and skill/);
  await page.evaluate(()=>renderFrame(11));
  const before=await page.locator('#work-progress').getAttribute('style');
  assert.equal(await page.locator('#answer').evaluate(e=>Number(e.style.opacity)),0);
  await page.evaluate(()=>renderFrame(16));
  assert.equal(await page.locator('#answer').evaluate(e=>Number(e.style.opacity)),1);
  assert.match(await page.locator('.ask-command').innerText(),/pika ask <exact-id> --jsonl/);
  assert.doesNotMatch(await page.locator('#original').innerText(),/Which dataset|published daily returns|second calculation/);
  assert.notEqual(await page.locator('#work-progress').getAttribute('style'),before);
  await page.evaluate(()=>renderFrame(21));
  assert.equal(await page.locator('#side-close').evaluate(e=>Number(e.style.opacity)),1);
  const answerBounds=await page.locator('#answer').boundingBox(),closeBounds=await page.locator('#side-close').boundingBox();
  assert(answerBounds.y+answerBounds.height<=closeBounds.y,'Side receipt must not cover the answer');
  assert.match(await page.locator('#original').innerText(),/Working/);
  for(const t of [44.5,48.5,52.5]){
   await page.evaluate(t=>renderFrame(t),t);const i=Math.floor((t-44)/4);
   assert.equal(await page.locator(`#step-${i}`).evaluate(e=>Number(e.style.opacity)),1);
   if(i<2)assert.equal(await page.locator(`#step-${i+1}`).evaluate(e=>Number(e.style.opacity)),0);
  }
  await page.evaluate(()=>renderFrame(56));const linkBefore=await page.locator('.repo').boundingBox();
  await page.evaluate(()=>renderFrame(58));assert.deepEqual(await page.locator('.repo').boundingBox(),linkBefore);
  await page.setViewportSize({width:960,height:540});await page.evaluate(()=>renderFrame(19));
  const bounds=await page.locator('#stage').boundingBox();assert(bounds.width<=961&&bounds.height<=541);
  assert.deepEqual(errors,[]);
  console.log('PASS: 60 seconds; four-second progressive opening; non-overprinting transition; persistent definition; scene layouts; isolated Q&A; continuing original work; progressive setup; stable CTA; deterministic seeking; responsive stage');
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
