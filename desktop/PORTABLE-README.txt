Kroma Desktop 0.3.0 - Windows x64 便携版
=====================================

使用方法
1. 解压整个文件夹，保留 Kroma Desktop.exe 和 WebView2Loader.dll 在同一目录。
2. 双击 Kroma Desktop.exe 启动，不需要运行开发服务器或安装命令行程序。
3. 首次使用时，在“模型与设置”中添加 API 连接；之后可直接在会话窗口切换模型。
4. 如果缺少 WebView2 Runtime，请通过 Microsoft 官方网站安装 WebView2 Evergreen Runtime。

数据保护
- 聊天历史、模型配置和插件状态保留在用户原来的 Kroma 数据目录；安装/解压此便携版不会删除它们。
- 已保存的模型密钥不会打包到这个 ZIP 中；ZIP 只包含程序、必需 DLL 和本说明。
- 请在退出旧版 Kroma Desktop 后启动新版，避免重复打开同一会话产生文件锁竞争。

说明
- 这是未进行代码签名的本地构建。Windows 可能提示未知发布者，请仅运行自己信任来源的版本。
- 运行 Agent 工具可能修改工作目录里的真实文件。完成任务后请在“工作区变更”里审查结果。
- 如需彻底卸载便携版，退出程序后删除解压的文件夹；这不会自动删除个人会话与配置。

项目: https://github.com/Polaris591/kroma
