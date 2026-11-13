import {installDesktopFixture} from './fixtures/desktop.mjs';

/** Real components, synthetic host. These are role/contrast/layout checks,
 * not a claim that pixel-perfect screenshots or native IMEs were tested. */
export async function verifyVisualPolish(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [], errors = [];
  const check = (condition, message) => { if (!condition) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page, {empty:true, plugins:[
    {id:'notes',name:'工作笔记',description:'整理项目笔记与常用参考',enabled:true,project:false,version:'1.2',
      settings:{api_key:'••••',limit:20,enabled:true},secret_settings:['api_key'],set_secrets:['api_key']},
    {id:'other',name:'参考资料',description:'收藏相关资料',enabled:false,project:true,version:'1.0',settings:{note:'合成资料'}},
  ]});
  await page.setViewportSize({width:1280,height:850});
  await page.goto(baseURL);
  const editor = page.getByRole('textbox',{name:'输入任务',exact:true});
  await editor.waitFor();
  await page.waitForFunction(()=>!document.querySelector('.loading-screen') && document.querySelector('.new-chat')?.disabled===false);
  const count = command => page.evaluate(command => window.__KROMA_UI_FIXTURE__.calls.filter(call=>call.command===command).length,command);
  const settle = () => page.evaluate(() => Promise.all(document.getAnimations()
    .filter(animation=>animation.effect?.getTiming().iterations!==Infinity)
    .map(animation=>animation.finished.catch(()=>{}))));
  async function navigate(name) {
    const target=page.getByRole('button',{name,exact:true});
    if (!await target.isVisible()) await page.getByRole('button',{name:'展开侧边栏',exact:true}).click();
    await target.click();
  }
  const categories=page.getByRole('navigation',{name:'设置分类',exact:true});
  const section = name => categories.getByRole('button',{name,exact:true}).click();
  async function inside(locator,label) {
    const box=await locator.boundingBox(), viewport=page.viewportSize();
    check(box && box.x>=-1 && box.x+box.width<=viewport.width+1,`${label} 横向越界`);
  }
  // Canvas resolves modern color-mix/light-dark colours and alpha, compositing
  // ancestor backgrounds before calculating actual text contrast.
  async function appearance(locator,pseudo=null) {
    await settle();
    return locator.evaluate((node,pseudo)=>{
      const context=document.createElement('canvas').getContext('2d');
      context.canvas.width=context.canvas.height=1;
      context.fillStyle='#fff';context.fillRect(0,0,1,1);
      const ancestors=[];for(let entry=node;entry;entry=entry.parentElement)ancestors.push(entry);
      for(const entry of ancestors.reverse()) { context.fillStyle=getComputedStyle(entry).backgroundColor;context.fillRect(0,0,1,1); }
      const background=Array.from(context.getImageData(0,0,1,1).data).slice(0,3);
      context.fillStyle=getComputedStyle(node,pseudo).color;context.fillRect(0,0,1,1);
      const text=Array.from(context.getImageData(0,0,1,1).data).slice(0,3);
      const luminance=rgb=>rgb.map(v=>v/255).map(v=>v<=.04045?v/12.92:((v+.055)/1.055)**2.4)
        .reduce((sum,v,index)=>sum+v*[.2126,.7152,.0722][index],0);
      const a=luminance(background),b=luminance(text);
      return {background,text,ratio:(Math.max(a,b)+.05)/(Math.min(a,b)+.05),scheme:getComputedStyle(node).colorScheme};
    },pseudo);
  }
  async function readable(locator,label,pseudo=null) {
    const result=await appearance(locator,pseudo);
    check(result.ratio>=4.5,`${label} 对比度不足：${JSON.stringify(result)}`);
    return result;
  }
  async function present(colors) {
    await page.evaluate(colors=>{const f=window.__KROMA_UI_FIXTURE__;return f.present({...f.presentation,colors});},colors);
    await settle();
  }

  const sends=await count('send_message');
  await page.getByRole('button',{name:/^检查项目/}).click();
  check((await editor.inputValue()).includes('检查当前项目'),'示例任务未填入草稿');
  check(await count('send_message')===sends,'示例任务未经确认就自动执行');
  await page.waitForFunction(()=>document.querySelector('.send-button')?.disabled===false);
  const send=await readable(page.locator('.send-button'),'默认发送按钮');
  check(Math.max(...send.background)-Math.min(...send.background)<16,'默认主操作仍然被强调色大面积染色');
  const brand=await readable(page.locator('.sidebar-brand-name'),'品牌文字');
  check(Math.abs(brand.text[0]-brand.text[1])<16,'品牌文字变成了强烈的装饰色');
  await readable(page.locator('.welcome > p'),'欢迎说明');
  passed.push('默认配色以中性主操作为主，示例只填草稿、不自动执行，正文与说明可读');

  await navigate('设置');
  check(await page.locator('.model-settings-layout').evaluate(node=>getComputedStyle(node).gridTemplateColumns.split(' ').length)===2,'宽窗口没有区分配置列表与编辑区域');
  await page.locator('.settings-editor-heading .connected').waitFor();
  const connections=await count('connect');
  await page.locator('.profile-switch button').filter({hasText:'local'}).click();
  check(await count('connect')===connections,'选择配置意外连接了模型');
  check(await page.locator('.settings-editor-heading .connected').count()===0,'查看未连接配置仍显示当前连接');
  const model=page.locator('.settings-form').getByLabel('模型 ID',{exact:true});
  const originalModel=await model.inputValue();
  await model.fill('仅用于检查草稿状态');
  await page.locator('.settings-editor-heading .pending').waitFor();
  await page.getByRole('button',{name:'撤销未保存修改',exact:true}).click();
  check(await model.inputValue()===originalModel,'新布局丢失了草稿撤销');
  const credential=page.locator('#settings-api-key');
  await credential.scrollIntoViewIfNeeded();
  const credentialBox=await credential.boundingBox();
  const actionsBox=await page.locator('.settings-action-area').boundingBox();
  check(credentialBox.y+credentialBox.height<=actionsBox.y,'保存操作遮挡了认证输入');
  await readable(page.locator('.settings-field-section label').first(),'表单标签');
  passed.push('模型选择与连接分离，当前连接和未保存状态准确，操作区不遮挡字段');

  await section('插件');
  const secret=page.locator('.config-fields input[type=password]');
  await secret.waitFor();
  check(await page.locator('.plugin-row').first().getAttribute('aria-pressed')==='true','插件页未选中默认详情');
  await secret.fill('synthetic-unsaved-key');
  await page.locator('.plugin-row').first().click();
  check(await secret.inputValue()==='synthetic-unsaved-key','重复选择当前插件清空了密钥');
  await page.locator('.plugin-row').nth(1).click();
  await page.locator('.plugin-row').first().click();
  check(await secret.inputValue()==='synthetic-unsaved-key','切换插件丢失了未保存密钥');
  check(await page.evaluate(()=>Object.values(localStorage).every(value=>!String(value).includes('synthetic-unsaved-key'))),'未保存密钥被写入本机存储');
  await page.evaluate(()=>{window.__KROMA_UI_FIXTURE__.failures.plugin_action='合成保存失败';});
  await page.locator('.config-input').getByRole('button',{name:'保存',exact:true}).click();
  await page.locator('.plugins-page .settings-error').waitFor();
  check(await secret.inputValue()==='synthetic-unsaved-key','失败后的密钥丢失');
  await page.locator('.config-input').getByRole('button',{name:'保存',exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.config-fields input[type=password]').value==='');
  check(await page.locator('.error-banner').count()===0,'成功重试后仍显示上一次的错误');
  await readable(page.locator('.plugin-inspector .inspector-description'),'插件说明');
  await readable(secret,'深色密钥占位说明','::placeholder');
  passed.push('插件直接显示详情，重复选择和跨插件切换保留未保存密钥，成功后清空且不落盘');

  await present({background:'#f7f6f3'});
  check((await appearance(page.locator('.main-area'))).scheme==='light','浅色背景仍使用深色系统控件');
  await readable(page.locator('.plugins-page .section-heading p'),'浅色页面说明');
  await readable(page.locator('.plugins-page .primary'),'浅色主按钮');
  await readable(page.locator('.plugin-state.enabled'),'浅色启用状态');
  await readable(page.locator('.config-fields label').first(),'浅色字段标签');
  await readable(secret,'浅色密钥占位说明','::placeholder');
  await page.getByRole('button',{name:'卸载',exact:true}).click();
  const uninstall=page.getByRole('dialog',{name:'卸载这个插件？',exact:true});
  await uninstall.waitFor();
  await readable(uninstall.locator('.dialog-description'),'浅色确认说明');
  await readable(uninstall.locator('.dialog-danger'),'浅色破坏性操作');
  await page.keyboard.press('Escape');
  passed.push('仅覆盖浅色背景也能获得正确系统控件、正文、说明、按钮和确认框对比度');

  await present({background:'#f7f6f3',accent:'#322060'});
  const darkAccent=await readable(page.locator('.plugins-page .primary'),'深色自定义强调按钮');
  check(darkAccent.text.every(value=>value>240),'深色强调按钮没有改用浅色文字');
  await present({accent:'#e7c66a'});
  await readable(page.locator('.plugins-page .primary'),'浅色自定义强调按钮');
  await present({});
  check((await appearance(page.locator('.main-area'))).scheme==='dark','移除主题未恢复默认原生控件');
  await readable(page.locator('.plugins-page .primary'),'恢复后的主按钮');
  passed.push('自定义强调色的按钮文字保持可读，移除主题完整恢复默认颜色与控件');

  for(const viewport of [{width:780,height:560},{width:380,height:700}]) {
    await page.setViewportSize(viewport);
    await section('模型');
    check(await page.locator('.model-settings-layout').evaluate(node=>getComputedStyle(node).gridTemplateColumns.split(' ').length)===1,'窄窗口仍然并排拥挤编辑区');
    await inside(page.locator('.settings-form'),'模型表单');
    const inputs=page.locator('.settings-form .field-grid input');
    for(let index=0;index<await inputs.count();index++)if(await inputs.nth(index).isVisible())await inside(inputs.nth(index),'模型字段');
    await navigate('返回对话');
    await inside(page.locator('.welcome-prompts'),'示例任务');
    await inside(page.locator('.composer-outer'),'输入区域');
    const input=await editor.boundingBox();
    check(input.y>=0 && input.y+input.height<=viewport.height,'欢迎页把输入框挤出了窗口');
    await navigate('设置');
  }
  passed.push('780×560 与 380px 窗口使用单列编辑，示例、输入与表单无横向越界');

  await page.emulateMedia({forcedColors:'active'});
  await section('工具');
  const search=page.getByRole('searchbox',{name:'搜索能力',exact:true});
  await search.focus();
  check(await search.evaluate(node=>getComputedStyle(node).outlineStyle==='solid' && parseFloat(getComputedStyle(node).outlineWidth)>=2),'高对比度模式缺少明确焦点');
  check(await categories.locator('[aria-current]').evaluate(node=>getComputedStyle(node).outlineStyle==='solid'),'高对比度模式的选中状态只依赖底色');
  await readable(page.locator('.capability-row label').first(),'高对比度能力文字');
  await section('插件');
  await page.locator('.switch.enabled').waitFor();
  check(await page.locator('.switch.enabled').evaluate(node=>getComputedStyle(node).forcedColorAdjust==='none' && getComputedStyle(node).backgroundColor!==getComputedStyle(node.firstElementChild).backgroundColor),'高对比度模式无法区分开关轨道与滑块');
  await page.emulateMedia({forcedColors:'none'});
  passed.push('系统高对比度模式保留焦点、选中轮廓、可读文字和可辨识的开关');
  check(errors.length===0,`未捕获的页面错误：${errors.join('; ')}`);
  return {passed,runtimeErrors:errors};
}
