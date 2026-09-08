const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const {pathToFileURL}=require('node:url');
const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright-core');
(async()=>{
 const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_PATH});
 try{
  const page=await browser.newPage({viewport:{width:1920,height:1080}}),errors=[];
  page.on('pageerror',e=>errors.push(String(e)));
  await page.goto(pathToFileURL(path.join(__dirname,'index.html')).href+'?render=1');
  for(const t of [1,4,8,13,22,34,44,53,59,62,68]){
   await page.evaluate(t=>window.renderFrame(t),t);
   const problems=await page.evaluate(()=>[...document.querySelectorAll('#scene h1,#scene h2,.terminal,.answer-card,.answer,.question,.expert-card,.hook-match,.hook-payoff,.search-panel,.decision-card,.under-caption,.three,.closing .note')].flatMap(el=>{
    const b=el.getBoundingClientRect();return b.left<0||b.top<0||b.right>1921||b.bottom>1081||el.scrollWidth>el.clientWidth+2?[el.className||el.tagName]:[];
   }));
   assert.deepEqual(problems,[],`Layout at ${t}s`);
   if(t===59)assert.match(await page.locator('.terminal pre').innerText(),/The provider requested permission/);
   if(t===62)assert.match(await page.locator('.terminal pre').innerText(),/provider reported completion/);
   if(t===8){
    assert.equal(await page.locator('#hook-reply').evaluate(e=>getComputedStyle(e).opacity),'1');
    assert.equal(await page.locator('#hook-payoff').evaluate(e=>getComputedStyle(e).opacity),'1');
    assert.match(await page.locator('#hook-reply').innerText(),/billing owns mutations/);
   }
   if(t===22){
    assert.match(await page.locator('.expert-card').innerText(),/Matched on scope \+ name/);
    assert.match(await page.locator('.expert-card').innerText(),/DESIGN.md/);
   }
   if(t===34){
    assert.equal(await page.locator('.question').innerText(),await page.evaluate(()=>PIKA_CONSULTATION.question));
    assert.equal(await page.locator('.answer').innerText(),await page.evaluate(()=>'“'+PIKA_CONSULTATION.answer+'”'));
   }
  }
  await page.evaluate(()=>window.renderFrame(34));const before=await page.locator('#scene').innerHTML();
  await page.evaluate(()=>window.renderFrame(9));await page.evaluate(()=>window.renderFrame(34));
  assert.equal(await page.locator('#scene').innerHTML(),before,'Seeking must be deterministic');
  assert.deepEqual(errors,[]);
  const source=['board.js','consultation.js','discovery.js','index.html'].map(file=>fs.readFileSync(path.join(__dirname,file),'utf8')).join('\n');
  assert(!/\/Users\/|\/mnt\/|master_|@rs6|019ff5b2|366df21d|01a07f02/.test(source),'No real inventory identifiers');
  assert.match(source,/synthetic inventory/);
  const proof=await page.evaluate(()=>PIKA_CONSULTATION.proof);
  assert.equal(proof.parentTranscriptUnchanged,true);
  assert.equal(proof.projectFilesUnchanged,true);
  assert.equal(proof.cleanupConfirmed,true);
  console.log('PASS: 11 layouts, payoff visible before 10s, discovery card, board state transition, recorded answer, scoped proof, deterministic seek, browser errors, fixture privacy');
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
