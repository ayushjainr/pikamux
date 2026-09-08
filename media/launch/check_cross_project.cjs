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
  await page.goto(pathToFileURL(path.join(__dirname,'cross-project.html')).href+'?render=1');
  for(const t of [2,4,7,8,9,16,25,36,44,47,56,59]){
   await page.evaluate(t=>window.renderFrame(t),t);
   const problems=await page.evaluate(()=>[...document.querySelectorAll('#scene h1,#scene h2,#scene h3,.panel,.quote,.code,.task,.edition,.note,.dashboard,.repo,.caveat')].flatMap(el=>{
    const b=el.getBoundingClientRect();return b.left<0||b.top<0||b.right>1921||b.bottom>1081||el.scrollWidth>el.clientWidth+2?[el.className||el.tagName]:[];
   }));
   assert.deepEqual(problems,[],`Layout at ${t}s`);
   assert.match(await page.locator('.edition').innerText(),/ILLUSTRATIVE DEMO/);
   if(t===8){assert.equal(await page.locator('#early-card').evaluate(e=>getComputedStyle(e).opacity),'1');assert.match(await page.locator('#expert-card').innerText(),/Weekly report exporter/);}
   if(t===25)assert.equal(await page.locator('.quote').innerText(),await page.evaluate(()=>'“'+PIKA_CROSS_PROJECT.question+'”'));
   if(t===36)assert.equal(await page.locator('.quote').innerText(),await page.evaluate(()=>'“'+PIKA_CROSS_PROJECT.answer+'”'));
   if(t===47)assert.match(await page.locator('#passed').innerText(),/Data connection: passed/);
   if(t===56)assert.match(await page.locator('.caveat').innerText(),/ayushjainr.com/);
  }
  for(const t of [3,8,36,45,56]){
   await page.evaluate(t=>window.renderFrame(t),t);const before=await page.locator('#scene').innerHTML();
   await page.evaluate(()=>window.renderFrame(15));await page.evaluate(t=>window.renderFrame(t),t);
   assert.equal(await page.locator('#scene').innerHTML(),before,`Deterministic seek at ${t}s`);
  }
  await page.setViewportSize({width:960,height:540});await page.evaluate(()=>window.renderFrame(36));
  const stage=await page.locator('#stage').boundingBox();assert(stage.width<=961&&stage.height<=541);
  const source=['cross-project.html','cross-project.js'].map(f=>fs.readFileSync(path.join(__dirname,f),'utf8')).join('\n');
  assert(!/\/Users\/|\/mnt\/|01a07f|master_|AI-assisted development|ACTUAL ANSWER|REAL AGENT RUNS|Verified in this capture/.test(source),'No private identifiers or false recording/development claims');
  assert.deepEqual(errors,[]);
  console.log('PASS: 12 layouts, card before 10s, dialogue, outcome, attribution, synthetic labels, deterministic seek, scaled viewport and browser errors');
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
