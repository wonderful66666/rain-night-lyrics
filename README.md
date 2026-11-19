# 雨夜轻息取词器

Windows 桌面程序，为 Wallpaper Engine 工坊场景《雨夜轻息》（作品编号 `3842256065`）提供逐句歌词。适用于支持 Windows 系统媒体控制的播放器。此版为 Windows 10 1809 及以上、Windows 11 的 x64 版本。

## 使用

1. 在 Wallpaper Engine 中将订阅的《雨夜轻息》应用到桌面，打开壁纸属性中的「顶部音乐胶囊」。
2. 双击 `RainNightLyrics.exe`。窗口显示播放器、查词结果和连接状态，听歌时保持程序运行。
3. 在 B 站播放一首歌，或者用网易云音乐播放歌曲。等窗口显示「已把……的同步歌词送到壁纸」，回桌面查看。可以换歌、暂停，或把进度拖到另一个段落。
4. 关闭取词器窗口即可退出。再次打开程序时如已有实例，输入 `q` 并回车可结束正在运行的 Rust 取词器。

桌面订阅的场景里已经有接收入口。取词器按工坊编号识别正在使用这张壁纸的屏幕，支持 Steam 库放在不同磁盘以及多显示器。切换到其他壁纸时会停止向该屏幕发送歌词。

## 测试时

- 「等待 Windows 播放媒体」表示当前播放器还没有把媒体会话提供给 Windows。可以先播放歌曲，确认系统媒体控件能看到曲名，再回桌面。可用的网易云版本如果提供「系统媒体控制」选项，请打开该选项。
- B 站提供的是视频标题，有明确歌名的单曲最容易匹配。合集、翻唱、纯音乐或曲库缺词时，胶囊可能显示歌名。如果播放器没有提供时间轴，逐句同步取决于壁纸能否获得播放进度。
- 若程序显示已送到壁纸而画面静止，可回到桌面检查。Wallpaper Engine 设置为在其他窗口最大化时暂停，会同时暂停胶囊动画。
- 可以在程序目录执行 `.\RainNightLyrics.exe --check` 查看匹配屏幕数、媒体来源及时间轴状态，或执行 `.\RainNightLyrics.exe --stop` 退出正在运行的 Rust 实例。

## 数据如何流动

取词器从 Windows 当前媒体会话读取曲名、歌手、进度与播放状态。播放期间会向网易云曲库或 LRCLIB 发送曲名，音乐播放器提供歌手时可以一并查询。查到的带时间轴歌词和翻译在程序内存中缓存。来源和查词状态显示在窗口，程序不写听歌历史。

取词器通过本机 Wallpaper Engine 的 `applyProperties` 更新当前壁纸的 `capsuleautomaticallyrics` 属性。属性值是用 Base64 编码的 UTF-8 JSON，包含播放状态和带时间的 LRC 文本。换歌、跳转和暂停触发同步，正常播放时定期校时。[Wallpaper Engine 命令行文档](https://help.wallpaperengine.io/en/functionality/cli.html#apply-wallpaper-settings) · [Windows 媒体接口](https://learn.microsoft.com/en-us/uwp/api/windows.media.control.globalsystemmediatransportcontrolssessionmanager) · [LRCLIB 接口](https://lrclib.net/docs)

## 从源码构建

在 Windows x64 安装 Rust 的 MSVC 工具链和 Visual Studio C++ 构建工具。在工程目录运行以下命令，本机验证使用 Rust 1.93。

```powershell
cargo test --release --locked
cargo build --release --locked
```

可执行文件生成在 `target\x86_64-pc-windows-msvc\release\rain-night-lyrics.exe`。
