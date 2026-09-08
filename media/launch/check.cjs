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
  for(const t of [3,9,17,21,27,38,50,58,66]){
   await page.evaluate(t=>window.renderFrame(t),t);
   const problems=await page.evaluate(()=>[...document.querySelectorAll('#scene h1,#scene h2,.terminal,.answer-card,.node,.evidence,.install')].flatMap(el=>{
    const b=el.getBoundingClientRect();return b.left<0||b.top<0||b.right>1921||b.bottom>1081||el.scrollWidth>el.clientWidth+2?[el.className||el.tagName]:[];
   }));
   assert.deepEqual(problems,[],`Layout at ${t}s`);
   if(t===17)assert.match(await page.locator('.terminal pre').innerText(),/The provider requested permission/);
   if(t===21)assert.match(await page.locator('.terminal pre').innerText(),/provider reported completion/);
  }
  await page.evaluate(()=>window.renderFrame(38));const before=await page.locator('#scene').innerHTML();
  await page.evaluate(()=>window.renderFrame(9));await page.evaluate(()=>window.renderFrame(38));
  assert.equal(await page.locator('#scene').innerHTML(),before,'Seeking must be deterministic');
  assert.deepEqual(errors,[]);
  const source=fs.readFileSync(path.join(__dirname,'board.js'),'utf8');
  assert(!/\/Users\/|\/mnt\/|master_|@rs6|019ff5b2|366df21d/.test(source),'No real inventory identifiers');
  assert.match(source,/synthetic inventory/);
  console.log('PASS: 9 scene layouts, board state transition, deterministic seek, browser errors, fixture privacy');
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
