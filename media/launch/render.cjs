/* Deterministic browser frames -> ffmpeg; fresh isolated browser, no user profile. */
const fs = require('node:fs');
const path = require('node:path');
const {spawn} = require('node:child_process');
const {once} = require('node:events');
const {pathToFileURL} = require('node:url');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright-core');
const outArg=process.argv.indexOf('--out');
const pageArg=process.argv.indexOf('--page');
const source=path.resolve(__dirname,pageArg<0?'index.html':process.argv[pageArg+1]);
const out=path.resolve(outArg<0?path.join(__dirname,'../../dist/launch-film'):process.argv[outArg+1]);
const stills=process.argv.includes('--stills');
fs.mkdirSync(out,{recursive:true});
(async()=>{
 const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_PATH});
 let encoder;
 try{
  const page=await browser.newPage({viewport:{width:1920,height:1080},deviceScaleFactor:1});
  const errors=[];page.on('pageerror',e=>errors.push(String(e)));
  await page.goto(pathToFileURL(source).href+'?render=1');
  await page.evaluate(()=>document.fonts.ready);
  const film=await page.evaluate(()=>window.PIKA_FILM||{seconds:72,stills:[1,4,8,13,22,34,44,53,59,62,68],basename:'pika-promise-and-proof.mp4'});
  if(!Number.isInteger(film.seconds)||film.seconds<1||film.seconds>300||!Array.isArray(film.stills)||!film.stills.every(t=>Number.isFinite(t)&&t>=0&&t<film.seconds)||!/^pika-[a-z0-9-]+\.mp4$/.test(film.basename))throw Error('Invalid film metadata');
  const movie=path.join(out,film.basename),frames=film.seconds*30;
  if(!stills&&fs.existsSync(movie))throw Error('Output already exists; choose a new --out directory.');
  for(const t of film.stills){
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
  for(let frame=0;frame<frames;frame++){
   if(encoder.exitCode!==null)throw Error('Encoder stopped: '+logs);
   await page.evaluate(t=>window.renderFrame(t),frame/30);
   const jpg=await page.screenshot({type:'jpeg',quality:94});
   if(!encoder.stdin.write(jpg))await once(encoder.stdin,'drain');
   if(frame%300===0)console.log(`Rendered ${frame}/${frames} frames`);
  }
  encoder.stdin.end();const [code]=await completed;if(code)throw Error(logs);
  fs.writeFileSync(path.join(out,'render-receipt.json'),JSON.stringify({width:1920,height:1080,fps:30,seconds:film.seconds,frames,source:path.basename(source),audio:false,videoModel:false,pageErrors:errors,bytes:fs.statSync(movie).size},null,2)+'\n');
  console.log('Video ready: '+movie);
 }finally{if(encoder&&encoder.exitCode===null)encoder.kill();await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
