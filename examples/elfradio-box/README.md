# elfRadio BOX — 上位机使用说明

业余无线电控制盒 PC 上位机，通过 USB 串口与 ESP32-S3 控制盒通信，实现对 TYT TH-9800 电台的软件控制。

- **台长呼号**：VK7KSM
- **串口连接**：自动检测 Espressif USB VID（0x303A），或通过 `--port` 手动指定（如 `COM8` 等，以设备管理器实际显示为准）
- **通信协议**：`[0xAA][0x55][Type][LenLo][LenHi][Payload][CRC16-CCITT_Lo][CRC16-CCITT_Hi]`

---

## 快速开始

### 编译

```bash
cd /c/eh/examples/elfradio-box
cargo build --release
```

输出：`target/x86_64-pc-windows-msvc/release/elfradio-box.exe`

### 部署

```bash
cp target/x86_64-pc-windows-msvc/release/elfradio-box.exe "/c/Users/x/OneDrive/桌面/elfradio-box/"
```

### 运行方式

| 方式 | 行为 |
|---|---|
| **双击 exe** | 自动检测串口 → 显示主菜单 |
| **PowerShell / CMD** | 支持完整 CLI 指令（见下文）|

---

## 主菜单

双击或不带参数运行时显示主菜单：

```
  [1] 监听模式     实时监听、显示状态、录音、PTT 发射
  [2] 连接状态     ESP32 / 电台 / 声卡信息
  [3] 设置         TTS 声线 / 降噪默认值
  [4] 固件更新     联网检查并烧录最新 ESP32 固件
  [0] 退出
```

ESP32 未连接时程序不退出，等待连接直到 Ctrl+C。

---

## TUI 监听界面

主菜单选 **[1]**，或 CLI 直接进入：

```
elfradio-box.exe [--port <串口号>] monitor
```

### 界面布局

```
elfRadio  RIGHT MAIN  FM 433.550.000 MHz  HIGH  S3  PgUp/PgDn  OK 监听中  空闲
──────────────────────────────────────────────────────────────────────────────
┌─  001 RX  ─── 14:30:22 ──────────────────────────────────────────────────┐
│ LEFT  433.550.000  LOW  DCS                                               │
│  RX   OK  recordings/RX_20260415_143022_433_550_000_18s_seg1.wav (18s)   │
└────────────────────────────────────────────────────────────────────────────┘

──────────────────────────────────────────────────────────────────────────────
> 输入文字后按 Enter 发射（Tab 聚焦）
──────────────────────────────────────────────────────────────────────────────
L:左频  R:右频  M:MAIN  P:发射  F:文件  Tab:TTS  O:电源  ↑↓:旋钮  V+←→:音量  Q+←→:静噪  W+↑↓:功率  T+↑↓:亚音  N+←→:降噪  Esc:退出
Radio:OK PC:OK  L: VOL 60% / SQL 25%  R: VOL 0% / SQL 0%  DNR:--
```

### 键盘操作

| 按键 | 功能 |
|---|---|
| `L` | 输入 LEFT 侧频率（6 位数字，自动切 MAIN）|
| `R` | 输入 RIGHT 侧频率（6 位数字，自动切 MAIN）|
| `M` | 切换 MAIN 侧（LEFT ↔ RIGHT）|
| `P`（长按 0.3s）| PTT 发射，松开停止；标题栏倒计时 30s→0，到期自动停止 |
| `F` | 弹出文件选择对话框，PTT + 播放（支持 WAV/MP3/OGG/FLAC/AAC/M4A，最长 30s）|
| `Tab` | 聚焦底部 TTS 文字输入框（再按 Tab 或点击其他区域失焦）|
| `Enter`（输入框聚焦时）| TTS 合成输入的文字 → PTT 发射，录音保存到 `recordings/` |
| `O` + `Y` | 电台开关机（GPIO8 脉冲）|
| `↑` / `↓` | MAIN 侧旋钮步进（调频）|
| `←` / `→` | 音量 ±5%（裸键）|
| `V` + `←` / `→` | 音量 ±5%（修饰键模式，与裸键等效）|
| `Q` + `←` / `→` | 静噪 ±5% |
| `W` + `↑` / `↓` | 功率循环（LOW → MID → HIGH）|
| `T` + `↑` / `↓` | 亚音模式循环（OFF → ENC → T/R → DCS）|
| `N` + `←` / `→` | 降噪强度 ±10（0=关闭，10-100=启用 RNNoise）|
| `PgUp` / `PgDn` | 消息区翻页（历史无限保留，重连后不丢失）|
| `Esc` | 先清 TTS 输入框；再按退出 TUI，返回主菜单 |

### 标题栏状态

- 右侧：`空闲` / `RX 接收中`（蓝底）/ `TX 发射中 28s`（红底，含秒级倒计时）
- 倒计时 30s 到期自动停止发射（与 ESP32 PTT 看门狗同步）

### 自动 RX 录音

- 需要 CM108 USB Audio 输入设备（连接电台扬声器输出）
- BUSY 信号出现时自动开始录音，信号消失后 2 秒内无续发则保存
- 单次录音 ≥ 0.3 秒才保存，否则丢弃（防噪声误触发）
- 每 300 秒自动分段保存，防止单文件过大
- 文件保存到 `recordings/` 目录（WAV 48kHz 单声道）

### TTS 文字发射

1. 按 `Tab` 聚焦底部输入框（或鼠标点击）
2. 输入中英文文字（支持 ←→/Home/End/Del/Backspace）
3. 按 `Enter` → Edge TTS 合成（默认声线：zh-TW-HsiaoChenNeural）
4. 合成完毕后自动 PTT 发射，录音保存到 `recordings/`

声线可在主菜单 **[3] 设置** 中修改，持久化到 `elfradio-box.cfg`。

### 断线重连

TUI 常驻不退出。串口断开时底部显示"等待重连..."，重连后历史消息和滚动位置完整保留。

---

## CLI 命令行

### 串口说明

程序启动时**自动检测** Espressif USB 设备（VID 0x303A，排除 JTAG 口）。若检测失败或有多个串口，用 `--port` 手动指定：

```
elfradio-box.exe [--port <串口号>] <命令> [参数...]
```

> 以下示例中 `--port COM8` 替换为实际串口号，或省略让程序自动检测。

---

### 帮助

```bash
elfradio-box.exe help
```

打印完整帮助，**无需串口**，直接退出。

---

### TUI 监听模式

```bash
elfradio-box.exe --port COM8 monitor
```

跳过主菜单，直接进入 TUI 实时监听界面（功能完整，自动重连）。

---

### 频率设置

```bash
elfradio-box.exe --port COM8 set-freq <L|R> <NNNNNN>
```

- `NNNNNN`：6 位纯数字，单位 kHz，**无小数点**
- 自动切换 MAIN 到目标侧后逐位发送频率，等待电台响应后读取确认

```bash
elfradio-box.exe --port COM8 set-freq L 433550   # → 433.550 MHz
elfradio-box.exe --port COM8 set-freq R 145500   # → 145.500 MHz
elfradio-box.exe --port COM8 set-freq R 146525   # → 146.525 MHz（全国呼叫）
```

---

### 切换 MAIN 侧

```bash
elfradio-box.exe --port COM8 main <L|R>
```

```bash
elfradio-box.exe --port COM8 main L
elfradio-box.exe --port COM8 main R
```

---

### 旋钮步进

```bash
elfradio-box.exe --port COM8 knob <up|down> [N]
```

对 MAIN 侧步进 N 格（默认 1，最大 50），步进后显示当前频率。

```bash
elfradio-box.exe --port COM8 knob up        # +1 格
elfradio-box.exe --port COM8 knob down 5    # -5 格
elfradio-box.exe --port COM8 knob up 10     # +10 格
```

---

### 音量

```bash
elfradio-box.exe --port COM8 set-vol <0-100>
```

```bash
elfradio-box.exe --port COM8 set-vol 60
elfradio-box.exe --port COM8 set-vol 0     # 静音
```

---

### 静噪

```bash
elfradio-box.exe --port COM8 set-sql <0-100>
```

```bash
elfradio-box.exe --port COM8 set-sql 25
elfradio-box.exe --port COM8 set-sql 0     # 关闭静噪（全噪）
```

---

### 功率等级

```bash
elfradio-box.exe --port COM8 set-power <L|R> <low|mid|high>
```

程序循环按 LOW 键直到达到目标功率（最多 5 次）。

```bash
elfradio-box.exe --port COM8 set-power L low    # ≈5W
elfradio-box.exe --port COM8 set-power L mid    # ≈10-25W
elfradio-box.exe --port COM8 set-power L high   # ≈50W
```

---

### 亚音模式循环

```bash
elfradio-box.exe --port COM8 tone
```

每次发送 P3 键，循环切换：`OFF → ENC → T/R → DCS → OFF → ...`

---

### PTT 发射（定时）

```bash
elfradio-box.exe --port COM8 ptt <秒数>
```

范围 **1-30 秒**（ESP32 硬限制），发射期间每秒打印倒计时。

```bash
elfradio-box.exe --port COM8 ptt 5     # 发射 5 秒
elfradio-box.exe --port COM8 ptt 10    # 发射 10 秒
```

---

### 强制关闭 PTT

```bash
elfradio-box.exe --port COM8 ptt-off
```

PTT 卡死时紧急关闭。

---

### PTT + 播放音频文件

```bash
elfradio-box.exe --port COM8 ptt-tx <文件路径>
```

开启 PTT → 播放文件到 CM108 USB Audio 输出（电台麦克风输入）→ 播完自动松开。

- **支持格式**：WAV / MP3 / OGG / FLAC / AAC / M4A（via rodio/symphonia）
- **自动截断**：超过 30 秒的文件截断到 30 秒
- **多声道**：自动平均混合为单声道

```bash
elfradio-box.exe --port COM8 ptt-tx cq_call.wav
elfradio-box.exe --port COM8 ptt-tx announcement.mp3
```

---

### TTS 文字转语音发射

```bash
elfradio-box.exe --port COM8 tts [--voice <声线>] <文字>
```

在线调用 Microsoft Edge TTS 合成音频 → 保存到 `recordings/` → PTT 发射。

- **默认声线**：`zh-TW-HsiaoChenNeural`（台湾女声）
- **合成文件**：保存为 `recordings/YYYYMMDD_HHMMSS_tts.mp3`

```bash
elfradio-box.exe --port COM8 tts "各位好，这里是 VK7KSM，CQ CQ"
elfradio-box.exe --port COM8 tts --voice zh-CN-XiaoxiaoNeural "Hello, this is VK7KSM"
elfradio-box.exe --port COM8 tts --voice zh-CN-YunxiNeural "测试播报"
```

**常用声线：**

| 声线 | 语言 | 性别 |
|---|---|---|
| `zh-TW-HsiaoChenNeural` | 台湾中文 | 女（默认）|
| `zh-CN-XiaoxiaoNeural` | 普通话 | 女 |
| `zh-CN-YunxiNeural` | 普通话 | 男 |
| `en-US-GuyNeural` | 英文 | 男 |

---

### 电台开关机

```bash
elfradio-box.exe --port COM8 power-toggle
```

触发 GPIO8 → PC817 光耦脉冲（1.2 秒），模拟电源键。

---

### 固件更新

```bash
# 仅查询 GitHub 最新版本（不需要 OTG 串口，无需 --port）
elfradio-box.exe flash --check

# 交互式下载并烧录（需 UART 调试线，不需要 OTG 线）
elfradio-box.exe flash

# 全自动非交互（自动确认，适合脚本）
elfradio-box.exe flash --yes

# 指定 UART 烧录口（自动检测失败时用）
elfradio-box.exe flash --yes --flash-port COM9
```

- **不需要 OTG 线**（`--port` 参数对此命令无效）
- 需要 **UART 调试线**（CH343/CH340，常接 GPIO43/44）
- 烧录成功后自动记录版本到 `elfradio-box.cfg`
- `flash --check` 退出码：`0` = 已是最新，`2` = 有新版本

---

### 被动收听（CLI 录音监听）

```bash
elfradio-box.exe [--port COM8] listen [选项]
```

长时间运行，打滚动日志，BUSY 信号出现时自动保存 RX 录音到 `recordings/`。  
**多个终止条件同时有效，任意一个先触发即停止。**

| 选项 | 说明 |
|---|---|
| `-d`, `--duration <时长>` | 运行时长，如 `30m` `2h` `3600s` `1800`（默认秒）|
| `-n`, `--count <N>` | 录完 N 次信号后停止 |
| `-i`, `--idle <时长>` | 最后一次信号结束后 N 时间无新活动则停止 |
| `--audio` | 开启接收音频直通（CM108 → PC 耳机，默认关闭）|
| `Ctrl+C` | 优雅终止（先保存正在进行的录音再退出）|

```bash
# 无限监听，Ctrl+C 停止
elfradio-box.exe --port COM8 listen

# 运行 30 分钟后自动结束
elfradio-box.exe --port COM8 listen -d 30m

# 录满 5 次信号后停止
elfradio-box.exe --port COM8 listen -n 5

# 最多 1 小时，20 分钟无信号提前结束
elfradio-box.exe --port COM8 listen -d 1h --idle 20m

# 开耳机直通，运行 2 小时
elfradio-box.exe --port COM8 listen --audio -d 2h
```

**输出示例：**
```
[14:30:22] 开始监听  电台在线  LEFT:433.550.000 RIGHT:146.525.000  时长上限:30m
────────────────────────────────────────────────────────────────────────────
[14:30:45] ← LEFT 433.550.000 FM  S:7  录音中...
[14:31:03] ✓ [0001] 已保存: recordings/RX_20260415_143045_433_550_000_18s_seg1.wav  (18.0s)
[14:35:12] ← RIGHT 146.525.000 FM  S:5  录音中...
[14:35:21] ✓ [0002] 已保存: recordings/RX_20260415_143512_146_525_000_9s_seg1.wav  (9.2s)
[14:00:22]  30m  达到时长上限，自动结束
────────────────────────────────────────────────────────────────────────────
[15:00:22] 监听结束  运行时间: 30m  共保存录音: 2 次
```

---

## 错误处理

| 错误信息 | 原因 | 解决方法 |
|---|---|---|
| `打开 <串口号> 失败: 拒绝访问` | 串口被其他程序占用 | 关闭其他上位机实例或 miniterm |
| `未找到 ESP32 串口` | ESP32 未通电或驱动未安装 | 检查 USB 连接，查看设备管理器 |
| `未收到状态报告` | ESP32 固件未烧录或串口不对 | 检查固件版本，确认串口号 |
| `无法初始化音频录音: ...` | CM108 未连接或被占用 | 检查 USB Audio 声卡连接；listen/TUI 仍可显示状态 |
| `功率循环 5 次后未达到目标` | 电台通信异常 | 手动核查当前功率，重试 |
| `未检测到 UART 烧录口` | flash 命令找不到 CH343/CH340 | 插入 UART 调试线，或用 `--flash-port` 指定 |
| `音频播放失败: ...` | ptt-tx 文件格式不支持或路径有误 | 确认文件路径，支持格式：WAV/MP3/OGG/FLAC/AAC/M4A |

---

## 脚本示例

### 定时自动播报

```bash
#!/bin/bash
PORT=COM8
EXE="./elfradio-box.exe"

# 设置频率和功率
$EXE --port $PORT set-freq L 433550
$EXE --port $PORT set-power L low

# TTS 播报
$EXE --port $PORT tts "各位好，这里是 VK7KSM，CQ CQ，请回答"
```

### 定时收听守候（守频2小时，无信号10分钟后结束）

```bash
elfradio-box.exe --port COM8 listen -d 2h --idle 10m --audio
```

### 夜间录音（全自动，第二天查看）

```bash
elfradio-box.exe --port COM8 listen -d 8h
```

### 预设配置批处理

```bat
@echo off
set PORT=COM8
set EXE=elfradio-box.exe

echo 设置左侧：VHF 144MHz 中继频率
%EXE% --port %PORT% main L
%EXE% --port %PORT% set-freq L 145000
%EXE% --port %PORT% set-power L low
%EXE% --port %PORT% set-vol 70
%EXE% --port %PORT% set-sql 20

echo 完成！
```

### 固件自动更新（CI/自动化）

```bash
elfradio-box.exe flash --check
if [ $? -eq 2 ]; then
    echo "发现新版本，自动升级..."
    elfradio-box.exe flash --yes
fi
```

---

## 注意事项

1. **串口独占**：同一时刻只能有一个程序打开串口（OTG 口）。TUI/listen 运行时不能同时执行其他 CLI 命令。
2. **PTT 安全**：`ptt <秒>` 上限 **30 秒**（ESP32 看门狗硬限制）。超过 30 秒的发射需求请使用 `ptt-tx` 或 `tts`。
3. **频率格式**：`set-freq` 仅接受 6 位纯数字（kHz）。433.550 MHz → `433550`；145.000 MHz → `145000`。
4. **两条 USB 线**：OTG 线（GPIO19/20）用于上位机通信；UART 调试线（CH343）用于固件烧录和日志读取。两者独立，互不影响。
5. **flash 不需要 OTG 线**：`flash` 命令走 UART 调试线，不需要上位机通信串口连接，也不需要 `--port` 参数。
6. **上电顺序**：先 ESP32（USB），再电台（13.8V）。绝对不要在 ESP32 未通电时给电台上电（RJ-12 带电会倒灌损坏电路）。

---

## 配置文件

`elfradio-box.cfg`（与 exe 同目录）：

```ini
# elfRadio BOX 配置文件
tts_voice=zh-TW-HsiaoChenNeural    # TTS 合成声线
denoise_db=0                        # 降噪强度（0=关闭，10-100=启用）
firmware_version=v0.1.0             # 上次成功烧录的固件版本
```

通过主菜单 **[3] 设置** 修改并保存，或直接手动编辑。

---

## 开发信息

- **语言**：Rust（x86_64-pc-windows-msvc）
- **主要依赖**：`crossterm`（TUI）、`serialport`（串口）、`cpal`（音频）、`hound`（WAV）、`rodio/symphonia`（多格式音频）、`msedge-tts`（TTS）、`nnnoiseless`（RNNoise 降噪）、`rfd`（文件对话框）、`espflash`（固件烧录）、`ctrlc`（Ctrl+C 处理）、`ureq`（HTTP）
- **固件仓库**：`VK7KSM/ESP32_Control_Box`（GitHub）
- **ESP32 固件源码**：`C:\eh\`（Xtensa Rust）
- **通信协议文档**：`C:\eh\PC_API.md`

73 de VK7KSM
