# 雨夜轻息取词器

Windows 桌面程序，为 Wallpaper Engine 工坊场景《雨夜轻息》（作品编号 `3842256065`）提供逐句歌词。适用于支持 Windows 系统媒体控制的播放器。此版为 Windows 10 1809 及以上、Windows 11 的 x64 版本。

## 使用

1. 在 Wallpaper Engine 中将订阅的《雨夜轻息》应用到桌面，打开壁纸属性中的「顶部音乐胶囊」。
2. 双击 **RainNightLyrics.exe**，回到桌面。音乐胶囊右边的小圆圈就是取词器。还没打开壁纸时，可以在主屏幕顶部找到它。
3. 用 B 站或网易云音乐播放歌曲。有封面时圆圈会像唱片一样旋转，外圈显示播放进度。点一下圆圈可看查词状态，显示「歌词已同步」后，壁纸会跟着播放器换词。
4. 想退出时，点一下圆圈，再点 **关闭**。找不到圆圈时，再次双击程序会让现有圆圈展开。

单独使用这张壁纸时，退出会清空自动歌词并重新载入当前场景，音乐继续播放，胶囊回到歌名和歌手。你手动填入的歌词仍按壁纸设置显示。若壁纸在播放列表里，取词器会保留播放列表并通知当前场景回到歌名。处于暂停状态的播放列表有可能错过通知，恢复播放后可重新应用该场景。

按住圆圈拖动可以调整位置，右键选择 **回到默认位置** 可以复位。多屏幕使用相同场景时，圆圈跟随第一个匹配屏幕。Windows 关闭动画效果时，圆圈会简化过渡效果。

圆圈直径为 36 个逻辑像素。动画跟随圆圈所在屏幕的刷新率。静止或被最大化窗口遮住时会暂停动画绘制。想看当前目标刷新率和最近的实际绘制帧率，可在程序目录运行 `.\RainNightLyrics.exe --check`。

桌面订阅的场景里已经有接收入口。取词器按工坊编号识别正在使用这张壁纸的屏幕，支持 Steam 库放在不同磁盘以及多显示器。切换到其他壁纸时会停止向该屏幕发送歌词。

## 蓝牙耳机重连后恢复音柱

保持取词器运行，将 Wallpaper Engine 设置里的「录音设备」选为「默认」。AirPods 重新连接、拔插 USB 耳机或切换到扬声器后，程序会等默认输出稳定约 3 秒，再检测是否有声音。连续两次测到声音后，它会在使用《雨夜轻息》的屏幕上重新载入当前场景。耳机出声后，音柱可能再过几秒才出现，歌词会按播放器的当前进度继续。

暂停或静音时，程序会等到声音恢复。手动指定录音设备、使用其他壁纸或屏幕配置中标有播放列表时，会跳过自动重载。设备通知和音量峰值只在本机处理，取词器不保存音频。

## 测试时

- 「等待播放」表示当前没有可读取的媒体。可以先播放歌曲，确认系统媒体控件能看到曲名，再回桌面。可用的网易云版本如果提供「系统媒体控制」选项，请打开该选项。
- B 站提供的是视频标题，有明确歌名的单曲最容易匹配。合集、翻唱、纯音乐或曲库缺词时，胶囊可能显示歌名。如果播放器没有提供时间轴，逐句同步取决于壁纸能否获得播放进度。
- 若程序显示已送到壁纸而画面静止，可回到桌面检查。Wallpaper Engine 设置为在其他窗口最大化时暂停，会同时暂停胶囊动画。
- 可以在程序目录执行 `.\RainNightLyrics.exe --check` 查看匹配屏幕数、媒体来源及时间轴状态，或执行 `.\RainNightLyrics.exe --stop` 退出正在运行的 Rust 实例。

## 数据如何流动

取词器从 Windows 当前媒体会话读取曲名、歌手、进度、播放状态和播放器提供的封面。播放期间会向网易云曲库或 LRCLIB 发送曲名，播放器提供歌手时可以一并查询。歌词和封面缓存在内存里。封面缺失时，圆圈显示内置渐变唱片。

本机状态日志保存在 `%LOCALAPPDATA%\RainNightLyrics`，大小达到 256 KB 时轮换并保留上一份。日志只含运行状态和曲库来源。各屏幕的拖动偏移保存在 `%APPDATA%\RainNightLyrics\settings.json`。若想随程序携带这些数据，在 exe 旁创建一个名为 `portable-data` 的文件夹即可。

取词器通过本机 Wallpaper Engine 的 `applyProperties` 更新当前壁纸的 `capsuleautomaticallyrics` 属性。属性值是用 Base64 编码的 UTF-8 JSON，包含播放状态和带时间的 LRC 文本。换歌、跳转和暂停触发同步，正常播放时定期校时。[Wallpaper Engine 命令行文档](https://help.wallpaperengine.io/en/functionality/cli.html#apply-wallpaper-settings) · [Windows 媒体接口](https://learn.microsoft.com/en-us/uwp/api/windows.media.control.globalsystemmediatransportcontrolssessionmanager) · [LRCLIB 接口](https://lrclib.net/docs)

## 从源码构建

在 Windows x64 安装 Rust 的 MSVC 工具链和 Visual Studio C++ 构建工具。在工程目录运行以下命令，本机验证使用 Rust 1.93。

```powershell
cargo test --release --locked
cargo build --release --locked
```

可执行文件生成在 `target\x86_64-pc-windows-msvc\release\rain-night-lyrics.exe`。
