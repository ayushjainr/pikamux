/* Deterministic browser frames -> ffmpeg; fresh isolated browser, no user profile. */
const fs = require('node:fs');
const path = require('node:path');
const {spawn} = require('node:child_process');
const {once} = require('node:events');
const {pathToFileURL} = require('node:url');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright-core');
const outArg=process.argv.indexOf('--out');
const out=path.resolve(outArg<0?path.join(__dirname,'../../dist/launch-film'):process.argv[outArg+1]);
const stills=process.argv.includes('--stills');
const movie=path.join(out,'pika-alchemy-cut.mp4');
if(!stills&&fs.existsSync(movie))throw Error('Output already exists; choose a new --out directory.');
fs.mkdirSync(out,{recursive:true});
(async()=>{
 const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_PATH});
 let encoder;
 try{
  const page=await browser.newPage({viewport:{width:1920,height:1080},deviceScaleFactor:1});
  const errors=[];page.on('pageerror',e=>errors.push(String(e)));
  await page.goto(pathToFileURL(path.join(__dirname,'index.html')).href+'?render=1');
  await page.evaluate(()=>document.fonts.ready);
  for(const t of [4,12,24,35,42,47,51,57,66]){
   await page.evaluate(t=>window.renderFrame(t),t);
   await page.screenshot({path:path.join(out,`scene-${t}.png`)});
  }
  if(errors.length)throw Error(errors.join('\n'));
  if(stills){console.log('Stills ready: '+out);return;}
  encoder=spawn(process.env.FFMPEG_PATH||'ffmpeg',['-hide_banner','-loglevel','warning','-n',
   '-f','image2pipe','-framerate','30','-vcodec','mjpeg','-i','pipe:0','-an',
   '-vf','scale=in_range=pc:out_range=tv:out_color_matrix=bt709',
   '-c:v','libx264','-preset','fast','-crf','18','-pix_fmt','yuv420p',
   '-color_range','tv','-colorspace','bt709','-color_primaries','bt709','-color_trc','bt709',
   '-movflags','+faststart',movie],
   {stdio:['pipe','ignore','pipe']});
  let logs='';encoder.stderr.on('data',d=>logs+=d);encoder.stdin.on('error',()=>{});
  const completed=once(encoder,'close');
  for(let frame=0;frame<72*30;frame++){
   if(encoder.exitCode!==null)throw Error('Encoder stopped: '+logs);
   await page.evaluate(t=>window.renderFrame(t),frame/30);
   const jpg=await page.screenshot({type:'jpeg',quality:94});
   if(!encoder.stdin.write(jpg))await once(encoder.stdin,'drain');
   if(frame%300===0)console.log(`Rendered ${frame}/2160 frames`);
  }
  encoder.stdin.end();const [code]=await completed;if(code)throw Error(logs);
  fs.writeFileSync(path.join(out,'render-receipt.json'),JSON.stringify({width:1920,height:1080,fps:30,seconds:72,frames:2160,audio:false,videoModel:false,pageErrors:errors,bytes:fs.statSync(movie).size},null,2)+'\n');
  console.log('Video ready: '+movie);
 }finally{if(encoder&&encoder.exitCode===null)encoder.kill();await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
