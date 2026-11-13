import {installDesktopFixture} from './fixtures/desktop.mjs';

/** Real Svelte interactions with synthetic IPC only: no provider, credentials,
 * local conversations, plugin process or native window is used. */
export async function verifyDesignReview(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [], errors = [];
  const check = (condition, message) => { if (!condition) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page, {
    plugins: [{id:'design-test', name:'设计检查插件', description:'验证密钥、保存与失败恢复', enabled:true,
      project:false, version:'1', settings:{api_key:'••••', enabled:true, limit:5, note:'原来的备注'},
      secret_settings:['api_key'], set_secrets:['api_key']}],
    turnReview: {design: {
      checkpoints:{turns:[{turn:1, anchor:2}], diffs:[{turn:1, files:2, added:1, removed:1, truncated:false}]},
      files:{1:[{path:'empty.txt',status:'M',added:0,removed:0},{path:'normal.txt',status:'M',added:1,removed:1}]},
      diffs:{1:{'empty.txt':''}},
    }},
  });
  await page.setViewportSize({width:1280,height:850});
  await page.goto(baseURL);
  const editor = page.getByRole('textbox',{name:'输入任务',exact:true});
  await editor.waitFor();
  const count = command => page.evaluate(command => window.__KROMA_UI_FIXTURE__.calls.filter(call => call.command === command).length, command);
  const fail = (command, message) => page.evaluate(({command,message}) => {window.__KROMA_UI_FIXTURE__.failures[command]=message;}, {command,message});
  const hold = command => page.evaluate(command => window.__KROMA_UI_FIXTURE__.hold(command), command);
  const release = (command, error) => page.evaluate(({command,error}) => window.__KROMA_UI_FIXTURE__.release(command,error), {command,error});
  const idleSettings = () => page.waitForFunction(() => !document.querySelector('.settings-form fieldset')?.disabled);
  async function navigate(name) {
    const target=page.getByRole('button',{name,exact:true});
    if (!await target.isVisible()) await page.getByRole('button',{name:'展开侧边栏',exact:true}).click();
    await target.click();
  }
  async function assertInside(locator, message) {
    const box=await locator.boundingBox();
    const {width,height}=page.viewportSize();
    check(box && box.x>=-1 && box.y>=-1 && box.x+box.width<=width+1 && box.y+box.height<=height+1,message);
  }

  // Popovers must remain normal, keyboard-reachable details, not invisible
  // pointer-only blockers. A nested native dialog still owns its own Escape.
  const actions=page.getByRole('button',{name:'会话操作',exact:true});
  await actions.click();
  const actionMenu=page.getByRole('dialog',{name:'会话操作',exact:true});
  check(await actionMenu.evaluate(node=>node===document.activeElement),'会话菜单没有初始焦点');
  for (const name of ['导出会话','备份会话','导入会话备份']) check(await actionMenu.getByRole('button',{name,exact:true}).count()===1,`会话菜单缺少${name}`);
  await page.keyboard.press('Escape');
  check(await actions.evaluate(node=>node===document.activeElement),'Escape 没有返回会话操作入口');
  await actions.click();
  await editor.click();
  await actionMenu.waitFor({state:'hidden'});
  check(await editor.evaluate(node=>node===document.activeElement),'点击输入框后菜单抢回了焦点');
  await actions.click();
  await actionMenu.getByRole('button',{name:'备份会话',exact:true}).click();
  const backup=page.getByRole('dialog',{name:'备份完整会话',exact:true});
  await backup.waitFor();
  check(await actionMenu.count()===0,'备份确认出现后会话菜单仍然遮挡背景');
  await page.keyboard.press('Escape');
  passed.push('会话菜单保留全部操作，支持 Escape、外部点击和嵌套确认');

  await page.locator('.title-button').click();
  const title=page.getByRole('textbox',{name:'会话名称',exact:true});
  check(await title.evaluate(node=>node===document.activeElement),'重命名没有聚焦输入框');
  await title.fill('不应保存的名称');
  const renamed=await count('rename_session');
  await page.getByRole('button',{name:'取消重命名',exact:true}).click();
  check(await count('rename_session')===renamed,'取消重命名仍然写入了名称');
  passed.push('重命名自动聚焦，有明确取消入口，取消不写入');

  await navigate('设置');
  const form=page.locator('.settings-form');
  const url=form.getByLabel('API Base URL',{exact:true});
  const originalURL=await url.inputValue();
  await url.fill('not-a-url');
  const saves=await count('save_profile');
  await form.getByRole('button',{name:'仅保存',exact:true}).click();
  check(await count('save_profile')===saves,'仅保存绕过了 URL 校验');
  check(await url.evaluate(node=>node===document.activeElement),'校验失败没有指向错误字段');
  await url.fill(originalURL);
  await form.locator('summary').click();
  await form.locator('input[type="number"]').first().fill('1');
  await form.getByRole('button',{name:'仅保存',exact:true}).click();
  check(await count('save_profile')===saves,'仅保存绕过了数值范围校验');
  await form.getByRole('button',{name:'撤销未保存修改',exact:true}).click();
  check(await url.inputValue()===originalURL,'撤销草稿没有恢复已保存配置');
  check(await form.locator('input[type="number"]').first().inputValue()==='200000','撤销草稿没有恢复原数值');
  passed.push('仅保存同样执行原生表单校验，未保存修改可以撤销');

  await page.getByRole('button',{name:'添加模型',exact:true}).click();
  const add=page.getByRole('dialog',{name:'添加模型',exact:true});
  await add.getByLabel('模型 ID',{exact:true}).fill('design-review-model');
  await hold('connect');
  const connections=await count('connect');
  await add.getByRole('button',{name:'保存并连接',exact:true}).click();
  await page.waitForFunction(n=>window.__KROMA_UI_FIXTURE__.calls.filter(call=>call.command==='connect').length>n,connections);
  check(await add.locator('input:enabled, .protocol-pills button:enabled').count()===0,'连接期间仍能改变正在提交的字段或协议');
  await page.keyboard.press('Escape');
  check(await add.isVisible(),'连接期间 Escape 隐藏了表单');
  await release('connect','合成连接失败');
  await add.getByRole('alert').waitFor();
  check(await add.getByLabel('模型 ID',{exact:true}).inputValue()==='design-review-model','失败后模型 ID 丢失');
  await page.keyboard.press('Escape');
  passed.push('模型连接期间锁定表单，失败后保留输入并允许关闭');

  const categories=page.getByRole('navigation',{name:'设置分类',exact:true});
  await categories.getByRole('button',{name:'插件',exact:true}).click();
  await page.locator('.plugin-row').click();
  const plugin=page.locator('.plugins-page');
  const secret=plugin.locator('.config-fields input[type="password"]');
  check(await secret.count()===1,'插件密钥没有使用密码输入框');
  await secret.fill('synthetic-private-value');
  await fail('plugin_action','合成密钥保存失败');
  await plugin.getByRole('button',{name:'保存',exact:true}).click();
  await plugin.locator('.settings-error').waitFor();
  check(await secret.inputValue()==='synthetic-private-value','保存失败清除了尚未保存的密钥');
  await hold('plugin_action');
  await plugin.getByRole('button',{name:'保存',exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.config-fields').disabled);
  check(await plugin.locator('.config-fields input:enabled, .plugin-row:enabled').count()===0,'保存期间仍可切换字段或插件');
  await release('plugin_action');
  await page.waitForFunction(()=>!document.querySelector('.config-fields').disabled);
  check(await secret.inputValue()==='','成功保存后密钥仍留在输入框');
  passed.push('插件密钥遮蔽，保存串行执行，失败保留、成功清空敏感输入');

  const enabled=plugin.locator('.config-fields input[type="checkbox"]');
  await fail('plugin_action','合成开关保存失败');
  await enabled.click();
  await plugin.locator('.settings-error').waitFor();
  check(await enabled.isChecked(),'保存失败的布尔设置显示成已生效');
  await plugin.getByRole('button',{name:'卸载',exact:true}).click();
  const uninstall=page.getByRole('dialog',{name:'卸载这个插件？',exact:true});
  await fail('plugin_action','合成卸载失败');
  await uninstall.getByRole('button',{name:'卸载插件',exact:true}).click();
  await uninstall.getByRole('alert').waitFor();
  check(await uninstall.isVisible() && await plugin.locator('.plugin-row').count()===1,'卸载失败却关闭确认或移除了插件');
  await page.keyboard.press('Escape');
  passed.push('失败开关回滚，卸载失败留在确认框中并能重试');

  await categories.getByRole('button',{name:'工具',exact:true}).click();
  const capability=page.getByRole('searchbox',{name:'搜索能力',exact:true});
  await capability.waitFor();
  check(await capability.evaluate(node=>getComputedStyle(node).borderRadius)==='8px','能力搜索框没有使用统一控件样式');
  await navigate('返回对话');
  await editor.fill('审查后仍保留的草稿');
  async function refreshContext(report, level) {
    await page.evaluate(report=>{window.__KROMA_UI_FIXTURE__.state.snapshot.context=report;},report);
    await page.getByRole('button',{name:'选择模型与思考强度',exact:true}).click();
    await page.locator('.thinking-menu-row').filter({hasText:level}).click();
    await page.waitForFunction(()=>!document.querySelector('.model-menu'));
  }
  await refreshContext(null,'低');
  const unknown=page.getByRole('button',{name:'上下文容量未知，点击查看详情',exact:true});
  await unknown.waitFor();
  await unknown.click();
  const context=page.getByRole('dialog',{name:'上下文使用详情',exact:true});
  check((await context.locator('.context-big').innerText()).includes('尚无占用估算'),'缺少报告时伪装成已知占用');
  check((await context.locator('.context-kv').first().innerText()).includes('—'),'未知占用显示成了零');
  await page.keyboard.press('Escape');
  check(await unknown.evaluate(node=>node===document.activeElement),'关闭上下文没有返回入口');
  await refreshContext({window:10000,available_input:6000,output_reserve:2000,segments:[{key:'messages',tokens:4200}]},'高');
  const contextTrigger=page.getByRole('button',{name:/上下文占用估算 42%/});
  await contextTrigger.click();
  check((await context.innerText()).includes('1.8k tokens'),'可用输入空间忽略了独立输入上限');
  await context.getByRole('button',{name:/已加载/}).click();
  await page.getByRole('dialog',{name:'本会话已加载的能力',exact:true}).waitFor();
  await page.keyboard.press('Escape');
  check(await context.isVisible(),'关闭能力弹窗时错误地关闭了下层上下文');
  await page.keyboard.press('Escape');
  check(await contextTrigger.evaluate(node=>node===document.activeElement),'两层浮层关闭后焦点丢失');
  passed.push('能力控件一致，上下文未知不伪造数字，并尊重真实输入上限与嵌套焦点');

  // Empty text must have an honest empty state, not a blank diff line.
  await page.getByRole('button',{name:/^本轮改动 2 个/}).click();
  const diff=page.getByRole('dialog',{name:'本轮改动 · 第 1 轮',exact:true});
  await diff.getByText('没有可显示的差异',{exact:true}).waitFor();
  check(await diff.locator('.review-diff-pre').count()===0,'空差异渲染成了一行空代码');
  await fail('turn_file_diff','合成差异读取失败');
  await diff.locator('.review-file-row').filter({hasText:'normal.txt'}).click();
  await diff.getByRole('alert').waitFor();
  await diff.getByRole('button',{name:'重试',exact:true}).click();
  await diff.locator('.review-diff-pre').waitFor();
  check(await diff.locator('.review-diff-pre').getAttribute('tabindex')==='0','键盘无法进入差异滚动区');
  await page.keyboard.press('Escape');
  passed.push('空差异有明确状态，读取失败能原地重试，键盘可以滚动差异');

  await page.evaluate(async()=>{
    const f=window.__KROMA_UI_FIXTURE__;
    await f.present({...f.presentation,widgets:[{plugin_id:'design-test',id:'progress',placement:'above_composer',title:'进度',actions:[],blocks:[
      {type:'progress',label:'完成进度',value:1,max:2},
      {type:'keyvalue',rows:[{key:'第一项',value:'说明一'},{key:'第二项',value:'说明二'}]},
    ]}]});
  });
  const progress=page.getByRole('progressbar',{name:'完成进度',exact:true});
  await progress.waitFor();
  check(await progress.getAttribute('aria-valuenow')==='50','进度条没有可访问数值');
  const ratio=await progress.evaluate(node=>node.firstElementChild.getBoundingClientRect().width/node.getBoundingClientRect().width);
  check(Math.abs(ratio-.5)<.02,'进度条的 span 没有实际绘制 50% 宽度');
  check(await page.locator('.panel-kv').evaluate(node=>getComputedStyle(node).display)==='grid','多个键值对挤在同一行');
  await page.evaluate(()=>window.__KROMA_UI_FIXTURE__.present(window.__KROMA_UI_FIXTURE__.presentation));
  passed.push('插件进度条真实绘制，键值信息按行对齐并支持辅助技术');

  await fail('plugin_desktop_bundle','合成界面加载失败');
  await page.evaluate(()=>{
    const f=window.__KROMA_UI_FIXTURE__;
    return f.present({...f.presentation,desktop_surfaces:[{plugin_id:'design-test',title:'自定义界面',slot:'above_composer',height:100}]});
  });
  await page.locator('.plugin-desktop-error').waitFor();
  await page.locator('.plugin-desktop-error').getByRole('button',{name:'重新加载',exact:true}).click();
  await page.locator('.composer-desktop-slot iframe').waitFor();
  await page.evaluate(()=>window.__KROMA_UI_FIXTURE__.present(window.__KROMA_UI_FIXTURE__.presentation));
  passed.push('插件自定义界面加载失败可原地重试，不必退出页面或重启应用');

  await page.setViewportSize({width:380,height:700});
  const titleBox=await page.locator('.title-button').boundingBox();
  check(titleBox.width>80,'窄窗口仍然把会话标题挤成一两个字');
  await assertInside(page.locator('.topbar'),'窄窗口标题栏越界');
  await actions.click();
  await assertInside(actionMenu,'窄窗口会话菜单越界');
  await page.keyboard.press('Escape');
  await contextTrigger.click();
  await assertInside(context,'窄窗口上下文详情越界');
  await page.keyboard.press('Escape');
  await page.getByRole('button',{name:'展开侧边栏',exact:true}).click();
  const drawer=page.getByRole('dialog',{name:'会话与主导航',exact:true});
  await drawer.waitFor();
  check(await page.locator('.main-area').getAttribute('inert')!==null,'抽屉打开时背景仍可获得键盘焦点');
  for(let i=0;i<24;i++) {
    await page.keyboard.press(i%5===0?'Shift+Tab':'Tab');
    check(await drawer.evaluate(node=>node.contains(document.activeElement)),'Tab 离开了窄窗口导航');
  }
  await page.keyboard.press('Escape');
  check(await page.getByRole('button',{name:'展开侧边栏',exact:true}).evaluate(node=>node===document.activeElement),'关闭抽屉后焦点没有返回入口');
  await page.getByRole('button',{name:'展开侧边栏',exact:true}).click();
  await drawer.getByRole('button',{name:'设置',exact:true}).click();
  await drawer.waitFor({state:'hidden'});
  check(await page.locator('.main-area').getAttribute('inert')===null,'导航后抽屉没有释放背景');
  await categories.getByRole('button',{name:'模型',exact:true}).click();
  const formBox = await page.locator('.settings-form').boundingBox();
  check(formBox.x >= 0 && formBox.x + formBox.width <= 381,'窄窗口设置表单横向越界');
  check(await page.locator('.profile-switch').evaluate(node=>node.clientHeight <= innerHeight*.3+1 && node.scrollHeight>node.clientHeight), '多个模型配置在窄窗占满了整个页面');
  await page.locator('.settings-form summary').click();
  check(await page.locator('.advanced-settings .field-grid').evaluate(node=>getComputedStyle(node).gridTemplateColumns.split(' ').length)===1,'窄窗口高级字段仍挤在两列');
  passed.push('380px 标题可读、菜单不越界，导航抽屉锁定焦点并在导航后收起');

  await navigate('返回对话');
  check(await editor.inputValue()==='审查后仍保留的草稿','导航修改丢失了会话草稿');
  await page.emulateMedia({reducedMotion:'reduce'});
  await page.evaluate(async()=>{
    const f=window.__KROMA_UI_FIXTURE__;
    await f.present({...f.presentation,widgets:[{plugin_id:'design-test',id:'motion',placement:'above_composer',title:'motion',actions:[],blocks:[]}]});
    const marker=document.createElement('span'); marker.className='spin';marker.id='motion-check';document.querySelector('.composer-outer').append(marker);
  });
  check(await page.locator('#motion-check').evaluate(node=>getComputedStyle(node).animationIterationCount)==='1','减少动态效果模式仍在无限循环动画');
  await page.locator('#motion-check').evaluate(node=>node.remove());
  await page.emulateMedia({reducedMotion:'no-preference'});
  passed.push('草稿完整保留，减少动态效果模式不再高速循环动画');

  await page.setViewportSize({width:780,height:560});
  await editor.fill('仅用于布局检查的任务');
  await page.getByRole('button',{name:'发送任务',exact:true}).click();
  await page.locator('.stop-button').waitFor();
  for(let index=1;index<=4;index++) {
    await editor.fill(`布局检查后续任务 ${index}`);
    await editor.press('Control+Enter');
    await page.waitForFunction(n=>document.querySelectorAll('.followup-item').length===n,index);
  }
  await editor.fill('多行草稿\n'.repeat(16));
  await page.waitForFunction(()=>document.querySelector('.composer textarea').clientHeight<=124);
  const layout=await page.evaluate(()=>({
    bottom:document.querySelector('.composer-outer').getBoundingClientRect().bottom,
    conversation:document.querySelector('.transcript-region').clientHeight,
  }));
  check(layout.bottom<=561 && layout.conversation>40,'长输入与展开的任务队列挤掉了全部对话或停止按钮');
  await editor.fill('@app');
  await page.locator('.file-suggest-row').first().waitFor();
  const chosen=await editor.getAttribute('aria-activedescendant');
  await editor.evaluate(node=>node.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',isComposing:true,bubbles:true,cancelable:true})));
  check(await editor.getAttribute('aria-activedescendant')===chosen,'输入法方向键移动了文件建议');
  await assertInside(page.locator('.file-suggest'),'文件建议弹层越界');
  const latest=await page.locator('.composer-outer').boundingBox();
  check(latest.y+latest.height<=561,'文件建议把输入和停止按钮挤出了窗口');
  await editor.press('Escape');
  await page.evaluate(()=>window.__KROMA_UI_FIXTURE__.finish('cancelled'));
  passed.push('780×560 中长输入、队列和文件建议不挤掉对话，输入法方向键不误选');
  check(errors.length===0,`未捕获的页面错误：${errors.join('; ')}`);
  return {passed,runtimeErrors:errors};
}
